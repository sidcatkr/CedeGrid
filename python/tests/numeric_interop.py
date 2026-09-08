"""Real supervised worker and receiver for coordinator-persisted cross-SDK numbers."""
import math
import sys
import tempfile
from pathlib import Path
from cedegrid import Client, WorkerContext, stringify_json, parse_json
METADATA={'producer':'python','integer53':2**53+1,'unsignedMax':2**64-1,'signedMin':-2**63,
          'integerOne':1,'floatOne':1.0,'negativeZero':-0.0,'tiny':5e-324,'huge':1.7976931348623157e308,
          'nested':{'unicode':'한국어 日本語 Ελληνικά','array':[2**53+1,1.0,-0.0]}}
def main():
    mode,*args=sys.argv[1:]
    if mode=='worker':
        worker=WorkerContext.from_env()
        with tempfile.TemporaryDirectory(prefix='cedegrid-numeric-worker-') as directory:
            path=Path(directory)/'numbers.jsonl'
            path.write_text((stringify_json(METADATA)+'\n')*2048,encoding='utf-8')
            artifact=worker.artifact('숫자.jsonl',path)
            worker.checkpoint(METADATA,[artifact])
            print(stringify_json(worker.complete(METADATA,[artifact])))
    elif mode=='verify':
        config,task_id,*extra=args
        producer=extra[0] if extra else 'typescript'
        client=Client.from_config(config)
        submission=client.result(task_id)
        assert submission,'coordinator has not accepted the result'
        value=submission['result']['metadata']
        assert value['producer']==producer
        for key in ('integer53','unsignedMax','signedMin','integerOne'):
            assert type(value[key]) is int and value[key]==METADATA[key],(key,value[key])
        for key in ('floatOne','negativeZero','tiny','huge'):
            assert type(value[key]) is float and value[key]==METADATA[key],(key,value[key])
        assert math.copysign(1,value['negativeZero'])==-1
        assert value['nested']['unicode']==METADATA['nested']['unicode']
        assert value['nested']['array'][0]==2**53+1
        assert type(value['nested']['array'][1]) is float
        assert math.copysign(1,value['nested']['array'][2])==-1
        with tempfile.TemporaryDirectory(prefix='cedegrid-numeric-receiver-') as directory:
            assert len(submission['result']['artifacts'])==1
            path=Path(directory)/'numbers.jsonl'
            client.download(submission['result']['artifacts'][0],path)
            assert path.stat().st_size>256*1024
            assert parse_json(path.read_text(encoding='utf-8').splitlines()[0])['integer53']==2**53+1
        print(stringify_json({'status':'PASS','task_id':task_id,'generation':submission['generation'],
                              'producer':producer,'consumer':'python','metadata':value}))
    elif mode=='encode':print(stringify_json(METADATA))
    else:raise ValueError('expected worker, verify, or encode mode')
if __name__=='__main__':main()
