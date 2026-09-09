/* SPDX-License-Identifier: Apache-2.0
 * Linux: cc -std=c11 -O2 -Wall -Wextra -Werror gpu_counter.c -ldl -o gpu_counter
 * No CUDA toolkit, runtime library, Python, or PyTorch is required to compile.
 * Execution requires libcuda.so.1, one full GPU UUID, native worker protocol 2,
 * and gpu_qualification.py's watchdog. --self-test uses no GPU or native state.
 * CUDA Driver API: https://docs.nvidia.com/cuda/cuda-driver-api/
 */
#define _GNU_SOURCE
#define _DARWIN_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#include <ctype.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/syscall.h>
#endif

#define ELEMENTS 16384u
#define ROUNDS 4096u
#define BATCHES 64u
#define FRAME 16384u
#define CONTEXT_LIMIT (1024u * 1024u)
typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef void *CUstream;
typedef unsigned long long CUdeviceptr;
typedef struct { unsigned char bytes[16]; } CUuuid;

/* sm_52 PTX is JIT compiled by the installed driver for both qualified models.
 * Every output is checked; wraparound is deliberate modulo-2^32 arithmetic. */
static const char kernel_ptx[] =
    ".version 6.0\n.target sm_52\n.address_size 64\n"
    ".visible .entry counter(.param .u64 output, .param .u32 count,"
    " .param .u32 seed, .param .u32 batch, .param .u32 rounds) {\n"
    ".reg .pred %p<3>; .reg .b32 %r<12>; .reg .b64 %rd<4>;\n"
    "ld.param.u64 %rd1,[output]; ld.param.u32 %r1,[count];\n"
    "ld.param.u32 %r2,[seed]; ld.param.u32 %r3,[batch];\n"
    "ld.param.u32 %r4,[rounds]; mov.u32 %r5,%ctaid.x;\n"
    "mov.u32 %r6,%ntid.x; mov.u32 %r7,%tid.x;\n"
    "mad.lo.u32 %r8,%r5,%r6,%r7; setp.ge.u32 %p1,%r8,%r1; @%p1 bra DONE;\n"
    "mad.lo.u32 %r9,%r3,17,%r2; add.u32 %r9,%r9,%r8; mov.u32 %r10,0;\n"
    "LOOP: mad.lo.u32 %r9,%r9,1664525,1013904223; add.u32 %r10,%r10,1;\n"
    "setp.lt.u32 %p2,%r10,%r4; @%p2 bra LOOP;\n"
    "mul.wide.u32 %rd2,%r8,4; add.u64 %rd3,%rd1,%rd2; st.global.u32 [%rd3],%r9;\n"
    "DONE: ret; }\n";

struct Driver {
    void *library;
    CUresult (*init)(unsigned int);
    CUresult (*count)(int *);
    CUresult (*device)(CUdevice *, int);
    CUresult (*uuid)(CUuuid *, CUdevice);
    CUresult (*name)(char *, int, CUdevice);
    CUresult (*version)(int *);
    CUresult (*attribute)(int *, int, CUdevice);
    CUresult (*retain)(CUcontext *, CUdevice);
    CUresult (*release)(CUdevice);
    CUresult (*set_current)(CUcontext);
    CUresult (*load)(CUmodule *, const void *);
    CUresult (*unload)(CUmodule);
    CUresult (*function)(CUfunction *, CUmodule, const char *);
    CUresult (*allocate)(CUdeviceptr *, size_t);
    CUresult (*free_memory)(CUdeviceptr);
    CUresult (*copy_to_host)(void *, CUdeviceptr, size_t);
    CUresult (*launch)(CUfunction, unsigned, unsigned, unsigned, unsigned,
                       unsigned, unsigned, unsigned, CUstream, void **, void **);
    CUresult (*synchronize)(void);
};
static int64_t milliseconds(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value)) return -1;
    return value.tv_sec * INT64_C(1000) + value.tv_nsec / 1000000;
}
static int ready(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t left = deadline - milliseconds();
        if (left <= 0) { errno = ETIMEDOUT; return -1; }
        struct pollfd value = {fd, events, 0};
        int result = poll(&value, 1, (int)left);
        if (result > 0) {
            if (value.revents & events) return 0;
            errno = EPIPE; return -1;
        }
        if (result == 0) { errno = ETIMEDOUT; return -1; }
        if (errno != EINTR) return -1;
    }
}
static const char *identity(const char *name) {
    const char *value = getenv(name);
    if (!value || !*value || strlen(value) > 128 ||
        strspn(value, "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-") != strlen(value)) {
        fprintf(stderr, "Missing/invalid worker identity: %s\n", name); exit(2);
    }
    return value;
}
static uint64_t decimal(const char *value, uint64_t limit) {
    if (!value || !*value || strspn(value, "0123456789") != strlen(value)) return UINT64_MAX;
    char *end; errno = 0; unsigned long long parsed = strtoull(value, &end, 10);
    return errno || *end || parsed > limit ? UINT64_MAX : (uint64_t)parsed;
}
static void random_id(char output[33]) {
    unsigned char bytes[16]; size_t done = 0;
    int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (fd < 0) { perror("random identity"); exit(2); }
    while (done < sizeof(bytes)) {
        ssize_t count = read(fd, bytes + done, sizeof(bytes) - done);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) { perror("random identity"); exit(2); }
        done += (size_t)count;
    }
    close(fd);
    for (size_t i = 0; i < sizeof(bytes); i++) snprintf(output + i*2, 3, "%02x", bytes[i]);
}
static int connect_local(const char *path, int64_t deadline) {
    struct sockaddr_un address; memset(&address, 0, sizeof(address));
    if (!path || strlen(path) >= sizeof(address.sun_path)) { errno = EINVAL; return -1; }
    address.sun_family = AF_UNIX; memcpy(address.sun_path, path, strlen(path)+1);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    if (fcntl(fd,F_SETFD,FD_CLOEXEC) || fcntl(fd,F_SETFL,O_NONBLOCK)) { close(fd); return -1; }
    if (connect(fd,(struct sockaddr *)&address,sizeof(address))) {
        if (errno != EINPROGRESS || ready(fd,POLLOUT,deadline)) { close(fd); return -1; }
        int error = 0; socklen_t length = sizeof(error);
        if (getsockopt(fd,SOL_SOCKET,SO_ERROR,&error,&length) || error) { close(fd); errno=error; return -1; }
    }
    return fd;
}
static int read_line(int fd, char *response, size_t bound, int64_t deadline) {
    size_t used=0;
    while (used+1 < bound) {
        if (ready(fd,POLLIN,deadline)) return -1;
        ssize_t count=read(fd,response+used,bound-used-1);
        if (count < 0 && (errno==EAGAIN || errno==EINTR)) continue;
        if (count <= 0) return -1;
        used+=(size_t)count;response[used]=0;
        if (strchr(response,'\n')) return 0;
    }
    errno=EOVERFLOW;return -1;
}
static int exchange(const char *payload, char response[FRAME]) {
    int64_t deadline=milliseconds()+15000;
    int fd=connect_local(getenv("CEDEGRID_SUPERVISOR_SOCKET"),deadline);
    if (fd<0) return -1;
    size_t size=strlen(payload),offset=0;
    while (offset<size) {
        if (ready(fd,POLLOUT,deadline)) {close(fd);return -1;}
        ssize_t count=write(fd,payload+offset,size-offset);
        if (count<0 && (errno==EAGAIN || errno==EINTR)) continue;
        if (count<=0) {close(fd);return -1;} offset+=(size_t)count;
    }
    int result=read_line(fd,response,FRAME,deadline);close(fd);return result;
}

/* Small bounded JSON reader used only for native-generated external context and
 * replies. It traverses direct object members instead of searching nested text. */
struct Slice { const char *begin, *end; };
static const char *space(const char *p) {while (*p && isspace((unsigned char)*p)) p++;return p;}
static const char *skip_string(const char *p) {
    if (*p++!='"') return NULL;
    while (*p && *p!='"') {
        if ((unsigned char)*p<32) return NULL;
        if (*p=='\\') {if (!*++p) return NULL;}
        p++;
    }
    return *p ? p+1 : NULL;
}
static const char *skip_value(const char *p,unsigned depth) {
    if (depth>32) return NULL;
    p=space(p);
    if (*p=='"') return skip_string(p);
    if (*p=='{' || *p=='[') {
        char close=*p++=='{'?'}':']';p=space(p);if (*p==close) return p+1;
        for (;;) {
            if (close=='}') {p=skip_string(p);if (!p || *space(p)!=':') return NULL;p=space(p)+1;}
            p=skip_value(p,depth+1);if (!p) return NULL;p=space(p);
            if (*p==close) return p+1;
            if (*p++!=',') return NULL;
            p=space(p);
        }
    }
    const char *start=p;while (*p && !strchr(",]} \t\r\n",*p)) p++;
    return p>start?p:NULL;
}
static int member(const char *object,const char *key,struct Slice *value) {
    const char *p=space(object);if (*p++!='{') return -1;p=space(p);
    while (*p && *p!='}') {
        const char *start=p;const char *end=skip_string(p);if (!end) return -1;
        int match=(size_t)(end-start)==strlen(key)+2 && !memcmp(start+1,key,strlen(key));
        p=space(end);if (*p++!=':') return -1;p=space(p);
        const char *next=skip_value(p,0);if (!next) return -1;
        if (match) {value->begin=p;value->end=next;return 1;}
        p=space(next);if (*p=='}') break;if (*p++!=',') return -1;p=space(p);
    }
    return *p=='}'?0:-1;
}
static int number(const char *object,const char *key,uint64_t maximum,uint64_t *result) {
    struct Slice value;if (member(object,key,&value)!=1 || value.end-value.begin>20) return -1;
    char buffer[21];size_t size=(size_t)(value.end-value.begin);memcpy(buffer,value.begin,size);buffer[size]=0;
    *result=decimal(buffer,maximum);return *result==UINT64_MAX?-1:0;
}
static int text_value(const char *object,const char *key,char *output,size_t bound) {
    struct Slice value;if (member(object,key,&value)!=1 || *value.begin!='"' || value.end-value.begin<2) return -1;
    size_t size=(size_t)(value.end-value.begin-2);
    if (size>=bound || memchr(value.begin+1,'\\',size)) return -1;
    memcpy(output,value.begin+1,size);output[size]=0;return 0;
}
static int is_committed(const char *response) {
    struct Slice ok;char state[24];
    return member(response,"ok",&ok)==1 && ok.end-ok.begin==4 && !memcmp(ok.begin,"true",4)
        && !text_value(response,"state",state,sizeof(state)) && !strcmp(state,"committed");
}
static int register_watchdog(int standalone) {
#ifdef __linux__
    int pidfd=(int)syscall(SYS_pidfd_open,getpid(),0);
    if (pidfd<0) return -1;
    int64_t deadline=milliseconds()+5000;
    int fd=connect_local(getenv("CEDEGRID_QUALIFIER_SOCKET"),deadline);
    if (fd<0) {close(pidfd);return -1;}
    const char *task=standalone?"competition":identity("CEDEGRID_TASK_ID");
    const char *attempt=standalone?"competition":identity("CEDEGRID_ASSIGNMENT_ID");
    const char *generation=standalone?"0":identity("CEDEGRID_ATTEMPT_GENERATION");
    if (decimal(generation,INT64_MAX)==UINT64_MAX) {close(fd);close(pidfd);return -1;}
    char payload[1024];int size=snprintf(payload,sizeof(payload),
        "{\"op\":\"register\",\"token\":\"%s\",\"task_id\":\"%s\",\"assignment_id\":\"%s\",\"generation\":%s}\n",
        identity("CEDEGRID_QUALIFIER_TOKEN"),task,attempt,generation);
    if (size<0 || (size_t)size>=sizeof(payload)) {close(fd);close(pidfd);return -1;}
    struct iovec data={payload,(size_t)size};
    union {struct cmsghdr alignment;char bytes[CMSG_SPACE(sizeof(int))];} ancillary;
    memset(&ancillary,0,sizeof(ancillary));struct msghdr message;memset(&message,0,sizeof(message));
    message.msg_iov=&data;message.msg_iovlen=1;message.msg_control=ancillary.bytes;message.msg_controllen=sizeof(ancillary.bytes);
    struct cmsghdr *control=CMSG_FIRSTHDR(&message);control->cmsg_level=SOL_SOCKET;control->cmsg_type=SCM_RIGHTS;
    control->cmsg_len=CMSG_LEN(sizeof(int));memcpy(CMSG_DATA(control),&pidfd,sizeof(pidfd));
    if (ready(fd,POLLOUT,deadline) || sendmsg(fd,&message,0)!=size) {close(fd);close(pidfd);return -1;}
    close(pidfd);char response[1024];int result=read_line(fd,response,sizeof(response),deadline);close(fd);
    if (result || !strstr(response,"\"registered\":true")) return -1;
    return 0;
#else
    (void)standalone;errno=ENOTSUP;return -1;
#endif
}
static void coefficients(uint32_t rounds,uint32_t *multiplier,uint32_t *offset) {
    uint32_t a=1664525u,c=1013904223u,m=1,b=0;
    while (rounds) {if (rounds&1u) {b=a*b+c;m=a*m;}c=a*c+c;a=a*a;rounds>>=1;}
    *multiplier=m;*offset=b;
}
static uint64_t expected_batch(uint32_t seed,uint32_t batch,uint32_t count) {
    uint32_t m,b;coefficients(ROUNDS,&m,&b);uint64_t sum=0;
    for (uint32_t i=0;i<count;i++) sum+=(uint32_t)(m*(seed+batch*17u+i)+b);
    return sum;
}
static uint64_t expected_prefix(uint32_t seed,uint32_t completed) {
    uint64_t sum=0;for (uint32_t batch=0;batch<completed;batch++) sum+=expected_batch(seed,batch,ELEMENTS);return sum;
}
static int resume_context(uint32_t seed,uint32_t *completed,uint64_t *sum) {
    const char *path=getenv("CEDEGRID_CONTEXT");if (!path) return -1;
    int fd=open(path,O_RDONLY|O_CLOEXEC|O_NOFOLLOW|O_NONBLOCK);if (fd<0) return -1;
    struct stat metadata;if (fstat(fd,&metadata) || !S_ISREG(metadata.st_mode) || metadata.st_size<0 || metadata.st_size>CONTEXT_LIMIT) {close(fd);return -1;}
    char *bytes=calloc(CONTEXT_LIMIT+1,1);if (!bytes) {close(fd);return -1;}
    size_t used=0;ssize_t count;
    while ((count=read(fd,bytes+used,CONTEXT_LIMIT+1-used))>0) {used+=(size_t)count;if (used>CONTEXT_LIMIT) break;}
    close(fd);if (count<0 || used>CONTEXT_LIMIT) {free(bytes);return -1;}
    struct Slice resume,meta;int found=member(bytes,"resume",&resume);
    if (found!=1) {free(bytes);return -1;}
    if (resume.end-resume.begin==4 && !memcmp(resume.begin,"null",4)) {free(bytes);return 0;}
    uint64_t old_completed,old_seed,elements,rounds,batches;char checksum[17],algorithm[32];
    if (member(resume.begin,"metadata",&meta)!=1 || text_value(meta.begin,"algorithm",algorithm,sizeof(algorithm)) || strcmp(algorithm,"lcg32-v1") ||
        number(meta.begin,"completed_batches",BATCHES,&old_completed) || number(meta.begin,"seed",UINT32_MAX,&old_seed) ||
        number(meta.begin,"elements",ELEMENTS,&elements) || number(meta.begin,"rounds",ROUNDS,&rounds) ||
        number(meta.begin,"total_batches",BATCHES,&batches) || text_value(meta.begin,"checksum_hex",checksum,sizeof(checksum)) ||
        strlen(checksum)!=16 || strspn(checksum,"0123456789abcdef")!=16 || old_completed==0 ||
        old_completed>=BATCHES || old_seed!=seed || elements!=ELEMENTS || rounds!=ROUNDS || batches!=BATCHES) {free(bytes);return -1;}
    char *end;errno=0;*sum=strtoull(checksum,&end,16);*completed=(uint32_t)old_completed;
    int result=errno || *end || *sum!=expected_prefix(seed,*completed);free(bytes);return result?-1:0;
}
static int publish(const char *kind,uint32_t seed,uint32_t completed,uint32_t resumed,uint64_t sum,const char *gpu) {
    char request[33],publication[33],payload[4096],response[FRAME];random_id(request);random_id(publication);
    const char *generation=identity("CEDEGRID_ATTEMPT_GENERATION");if (decimal(generation,INT64_MAX)==UINT64_MAX) return -1;
    int size=snprintf(payload,sizeof(payload),
        "{\"version\":2,\"op\":\"publication_commit\",\"request_id\":\"%s\",\"publication_id\":\"%s\",\"token\":\"%s\",\"namespace_id\":\"%s\",\"session_id\":\"%s\",\"assignment_id\":\"%s\",\"generation\":%s,\"kind\":\"%s\",\"metadata\":{\"algorithm\":\"lcg32-v1\",\"seed\":%u,\"elements\":%u,\"rounds\":%u,\"total_batches\":%u,\"completed_batches\":%u,\"resumed_from\":%u,\"checksum_hex\":\"%016" PRIx64 "\",\"gpu_uuid\":\"%s\",\"gpu_verified_values\":%u},\"artifact_ids\":[]}\n",
        request,publication,identity("CEDEGRID_SUPERVISOR_TOKEN"),identity("CEDEGRID_NAMESPACE_ID"),identity("CEDEGRID_SESSION_ID"),identity("CEDEGRID_ASSIGNMENT_ID"),generation,kind,seed,ELEMENTS,ROUNDS,BATCHES,completed,resumed,sum,gpu,(completed-resumed)*ELEMENTS);
    if (size<0 || (size_t)size>=sizeof(payload)) return -1;
    if (exchange(payload,response) || !is_committed(response)) {
        /* An ambiguous response keeps the original operation ID for reconciliation. */
        fprintf(stderr,"publication uncertain id=%s request=%s\n",publication,request);return -1;
    }
    printf("{\"event\":\"gpu_progress\",\"kind\":\"%s\",\"publication_id\":\"%s\",\"completed_batches\":%u,\"checksum_hex\":\"%016" PRIx64 "\"}\n",kind,publication,completed,sum);fflush(stdout);
    return 0;
}
static int load_driver(struct Driver *driver) {
    memset(driver,0,sizeof(*driver));driver->library=dlopen("libcuda.so.1",RTLD_NOW|RTLD_LOCAL);
    if (!driver->library) {fprintf(stderr,"CUDA driver unavailable\n");return -1;}
#define LOAD(field,symbol) do {void *address=dlsym(driver->library,symbol);if (!address) {fprintf(stderr,"Missing CUDA symbol %s\n",symbol);return -1;}memcpy(&driver->field,&address,sizeof(address));} while(0)
    LOAD(init,"cuInit");LOAD(count,"cuDeviceGetCount");LOAD(device,"cuDeviceGet");LOAD(uuid,"cuDeviceGetUuid_v2");
    LOAD(name,"cuDeviceGetName");LOAD(version,"cuDriverGetVersion");LOAD(attribute,"cuDeviceGetAttribute");
    LOAD(retain,"cuDevicePrimaryCtxRetain");LOAD(release,"cuDevicePrimaryCtxRelease_v2");LOAD(set_current,"cuCtxSetCurrent");
    LOAD(load,"cuModuleLoadData");LOAD(unload,"cuModuleUnload");LOAD(function,"cuModuleGetFunction");
    LOAD(allocate,"cuMemAlloc_v2");LOAD(free_memory,"cuMemFree_v2");LOAD(copy_to_host,"cuMemcpyDtoH_v2");
    LOAD(launch,"cuLaunchKernel");LOAD(synchronize,"cuCtxSynchronize");
#undef LOAD
    return 0;
}
static void uuid_string(const CUuuid *uuid,char output[41]) {
    char *at=output;memcpy(at,"GPU-",4);at+=4;
    for (unsigned i=0;i<16;i++) {if (i==4 || i==6 || i==8 || i==10) *at++='-';snprintf(at,3,"%02x",uuid->bytes[i]);at+=2;}
}
static int select_device(struct Driver *driver,const char *selected,CUdevice *device) {
    int count=0;if (driver->init(0) || driver->count(&count) || count!=1) {fprintf(stderr,"Exactly one CUDA-visible GPU is required\n");return -1;}
    CUuuid uuid;char text[41];if (driver->device(device,0) || driver->uuid(&uuid,*device)) return -1;
    uuid_string(&uuid,text);if (strcmp(text,selected)) {fprintf(stderr,"Selected GPU UUID differs from CUDA device\n");return -1;}
    return 0;
}
static int self_test(void) {
    uint32_t m,b;coefficients(ROUNDS,&m,&b);
    for (uint32_t n=0;n<128;n++) {uint32_t value=n;for (unsigned r=0;r<ROUNDS;r++) value=value*1664525u+1013904223u;if (value!=(uint32_t)(m*n+b)) return 2;}
    struct Slice value;uint64_t number_value;
    if (member("{\"x\":{\"generation\":99},\"generation\":7}","generation",&value)!=1 ||
        number("{\"generation\":7}","generation",9,&number_value) || number_value!=7 ||
        is_committed("{\"ok\":false,\"x\":\"\\\"ok\\\":true\",\"state\":\"committed\"}")) return 2;
    printf("{\"status\":\"PASS\",\"scope\":\"cpu_reference_and_protocol_parser_only\",\"elements\":%u,\"rounds\":%u,\"seed\":7,\"batches\":%u,\"checksum_hex\":\"%016" PRIx64 "\",\"gpu_status\":\"NOT_RUN\"}\n",ELEMENTS,ROUNDS,BATCHES,expected_prefix(7,BATCHES));return 0;
}
int main(int argc,char **argv) {
    signal(SIGPIPE,SIG_IGN);
    if (argc==2 && !strcmp(argv[1],"--self-test")) return self_test();
    int probe=argc==3 && !strcmp(argv[1],"--probe");
    int standalone=argc==4 && !strcmp(argv[1],"--competition");
    if (!probe && !standalone && !(argc==3 && !strcmp(argv[1],"--seed"))) {fprintf(stderr,"usage: gpu_counter --self-test | --probe GPU-UUID | --seed UINT32 | --competition GPU-UUID SECONDS\n");return 2;}
    const char *selected=probe || standalone?argv[2]:getenv("CUDA_VISIBLE_DEVICES");
    if (!selected || strlen(selected)!=40 || strncmp(selected,"GPU-",4) || strspn(selected+4,"0123456789abcdef-")!=36) return 2;
    if (setenv("CUDA_VISIBLE_DEVICES",selected,1)) return 2;
    uint64_t parsed=probe || standalone?7:decimal(argv[2],UINT32_MAX);
    uint64_t seconds=standalone?decimal(argv[3],10):0;
    if (parsed==UINT64_MAX || (standalone && (seconds==0 || seconds==UINT64_MAX))) return 2;
    uint32_t seed=(uint32_t)parsed,completed=0,resumed=0;uint64_t sum=0;
    if (!probe && register_watchdog(standalone)) {perror("watchdog registration before CUDA");return 2;}
    if (!probe && !standalone && resume_context(seed,&completed,&sum)) {fprintf(stderr,"Invalid checkpoint continuation\n");return 2;}
    resumed=completed;struct Driver driver;if (load_driver(&driver)) return 3;CUdevice device;
    if (select_device(&driver,selected,&device)) {dlclose(driver.library);return 3;}
    if (probe) {
        int version=0,major=0,minor=0;char name[128]={0};
        if (driver.version(&version) || driver.attribute(&major,75,device) || driver.attribute(&minor,76,device) || driver.name(name,sizeof(name),device)) return 3;
        /* Known product names contain no JSON metacharacters. */
        for (char *p=name;*p;p++) if (*p=='"' || *p=='\\' || (unsigned char)*p<32) *p='_';
        printf("{\"status\":\"READY\",\"device_name\":\"%s\",\"driver_api\":%d,\"compute_capability\":[%d,%d],\"context_created\":false}\n",name,version,major,minor);
        dlclose(driver.library);return 0;
    }
    CUcontext context=NULL;CUmodule module=NULL;CUfunction kernel=NULL;CUdeviceptr output=0;
    uint32_t *host=malloc(ELEMENTS*sizeof(*host));int result=2;int64_t start=milliseconds();
#define CUDA(call) do {CUresult code=(call);if (code) {fprintf(stderr,"CUDA error %d at %s\n",code,#call);goto cleanup;}} while(0)
    if (!host) goto cleanup;
    CUDA(driver.retain(&context,device));CUDA(driver.set_current(context));CUDA(driver.load(&module,kernel_ptx));
    CUDA(driver.function(&kernel,module,"counter"));CUDA(driver.allocate(&output,ELEMENTS*sizeof(*host)));
    uint32_t multiplier,offset;coefficients(ROUNDS,&multiplier,&offset);
    while (standalone || completed<BATCHES) {
        if (milliseconds()-start>45000) {fprintf(stderr,"worker deadline exceeded\n");goto cleanup;}
        if (standalone && milliseconds()-start>=(int64_t)seconds*1000) {result=0;break;}
        uint32_t count=ELEMENTS,rounds=ROUNDS,batch=completed;void *arguments[]={&output,&count,&seed,&batch,&rounds};
        CUDA(driver.launch(kernel,(ELEMENTS+127)/128,1,1,128,1,1,0,NULL,arguments,NULL));
        CUDA(driver.synchronize());CUDA(driver.copy_to_host(host,output,ELEMENTS*sizeof(*host)));
        uint64_t batch_sum=0;
        for (uint32_t i=0;i<ELEMENTS;i++) {
            uint32_t expected=multiplier*(seed+batch*17u+i)+offset;
            if (host[i]!=expected) {fprintf(stderr,"GPU/CPU integer mismatch at batch=%u index=%u\n",batch,i);goto cleanup;}
            batch_sum+=host[i];
        }
        completed++;sum+=batch_sum;
        if (standalone) continue;
        const char *drain=getenv("CEDEGRID_DRAIN_FILE");
        if (drain && access(drain,F_OK)==0) {result=publish("checkpoint",seed,completed,resumed,sum,selected)?2:75;break;}
        if (completed==resumed+2 && completed<BATCHES && publish("checkpoint",seed,completed,resumed,sum,selected)) goto cleanup;
        /* Keep execution alive long enough for a 10-second lease-loss scenario. */
        struct timespec pause={0,350000000};while (nanosleep(&pause,&pause) && errno==EINTR) {}
    }
    if (!standalone && completed==BATCHES && result!=75) result=publish("result",seed,completed,resumed,sum,selected)?2:0;
cleanup:
    if (output && driver.free_memory(output)) result=2;
    if (module && driver.unload(module)) result=2;
    if (context && driver.release(device)) result=2;
    free(host);dlclose(driver.library);
    if (standalone && !result) {printf("{\"event\":\"competition_complete\",\"gpu_verified_values\":%" PRIu64 "}\n",(uint64_t)completed*ELEMENTS);fflush(stdout);}
    return result;
#undef CUDA
}
