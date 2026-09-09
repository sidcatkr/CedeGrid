/* SPDX-License-Identifier: Apache-2.0
 * cc -std=c11 -O2 -Wall -Wextra -Werror counter.c -o counter
 * Submit this executable with a single-process/no-escape contract. It requires
 * neither Python nor Node and publishes through the native supervisor socket.
 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static int64_t milliseconds(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    return now.tv_sec * INT64_C(1000) + now.tv_nsec / 1000000;
}
static int ready(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t remaining = deadline - milliseconds();
        if (remaining <= 0) { errno = ETIMEDOUT; return -1; }
        struct pollfd wait = { .fd = fd, .events = events, .revents = 0 };
        int result = poll(&wait, 1, (int)remaining);
        if (result > 0) return 0;
        if (result == 0) { errno = ETIMEDOUT; return -1; }
        if (errno != EINTR) return -1;
    }
}
/* Native-generated IDs/tokens use this alphabet; rejecting any other token
 * prevents accidental JSON injection in this dependency-free example. */
static const char *identity(const char *name) {
    const char *value = getenv(name);
    if (!value || !*value || strlen(value) > 128 || strspn(value, "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-") != strlen(value)) {
        fprintf(stderr, "Missing or invalid native worker identity: %s\n", name); exit(2);
    }
    return value;
}
static void random_id(char output[33]) {
    unsigned char bytes[16];
    int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (fd < 0 || read(fd, bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) { perror("request identity"); exit(2); }
    close(fd);
    for (size_t i = 0; i < sizeof(bytes); ++i) snprintf(output + 2*i, 3, "%02x", bytes[i]);
}
static int exchange(const char *payload, char response[8192]) {
    const char *path = getenv("CEDEGRID_SUPERVISOR_SOCKET");
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address)); address.sun_family = AF_UNIX;
    if (!path || strlen(path) >= sizeof(address.sun_path)) { errno = EINVAL; return -1; }
    memcpy(address.sun_path, path, strlen(path) + 1);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    if (fcntl(fd, F_SETFD, FD_CLOEXEC) || fcntl(fd, F_SETFL, O_NONBLOCK)) { close(fd); return -1; }
    int64_t deadline = milliseconds() + 15000;
    if (connect(fd, (struct sockaddr *)&address, sizeof(address)) && errno != EINPROGRESS) { close(fd); return -1; }
    size_t offset = 0, size = strlen(payload);
    while (offset < size) {
        if (ready(fd, POLLOUT, deadline)) { close(fd); return -1; }
        ssize_t written = write(fd, payload + offset, size - offset);
        if (written > 0) offset += (size_t)written;
        else if (written < 0 && errno != EINTR && errno != EAGAIN) { close(fd); return -1; }
    }
    offset = 0;
    for (;;) {
        if (ready(fd, POLLIN, deadline)) { close(fd); return -1; }
        ssize_t count = read(fd, response + offset, 8191 - offset);
        if (count <= 0) { if (count < 0 && (errno == EINTR || errno == EAGAIN)) continue; close(fd); return -1; }
        offset += (size_t)count; response[offset] = '\0';
        if (strchr(response, '\n')) { close(fd); return 0; }
        if (offset == 8191) { close(fd); errno = EOVERFLOW; return -1; }
    }
}
static int publish(const char *kind, uint64_t completed, uint64_t sum) {
    char request[33], publication[33], payload[2048], response[8192];
    random_id(request); random_id(publication);
    const char *generation = identity("CEDEGRID_ATTEMPT_GENERATION");
    if (strspn(generation, "0123456789") != strlen(generation)) return -1;
    int size = snprintf(payload, sizeof(payload),
        "{\"version\":2,\"op\":\"publication_commit\",\"request_id\":\"%s\",\"publication_id\":\"%s\",\"token\":\"%s\",\"namespace_id\":\"%s\",\"session_id\":\"%s\",\"assignment_id\":\"%s\",\"generation\":%s,\"kind\":\"%s\",\"metadata\":{\"completed\":%" PRIu64 ",\"sum\":%" PRIu64 "},\"artifact_ids\":[]}\n",
        request, publication, identity("CEDEGRID_SUPERVISOR_TOKEN"), identity("CEDEGRID_NAMESPACE_ID"), identity("CEDEGRID_SESSION_ID"), identity("CEDEGRID_ASSIGNMENT_ID"), generation, kind, completed, sum);
    if (size < 0 || (size_t)size >= sizeof(payload)) return -1;
    if (exchange(payload,response) || !strstr(response,"\"ok\":true") || !strstr(response,"\"state\":\"committed\"")) {
        /* Keep this original ID after an ambiguous ACK; do not invent a retry. */
        fprintf(stderr,"Publication requires reconciliation: %s (request %s)\n",publication,request); return -1;
    }
    return 0;
}
int main(int argc, char **argv) {
    uint64_t total = UINT64_C(1000000), sum = 0;
    if (argc > 2) return 2;
    if (argc == 2) { char *end = NULL; errno = 0; total = strtoull(argv[1],&end,10); if (errno || !end || *end || total == 0 || total > UINT64_C(100000000)) return 2; }
    const char *drain = getenv("CEDEGRID_DRAIN_FILE");
    for (uint64_t n = 1; n <= total; ++n) {
        sum += n;
        if (n % 100000 == 0 && drain && access(drain,F_OK) == 0) { return publish("checkpoint",n,sum) ? 2 : 75; }
        if (n == total / 2 && publish("checkpoint",n,sum)) return 2;
    }
    if (publish("result",total,sum)) return 2;
    printf("completed=%" PRIu64 " sum=%" PRIu64 "\n",total,sum);
    return 0;
}
