#!/usr/bin/env python3
"""Verify owned server fixtures and typed decoding before timed collection."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

from cases import CASES, parameters, query_for
from run import Server, Worker, binary_name


def smoke(work, pins):
    destination=work/'server-smoke.json'
    data=work/'smoke-data'
    data.mkdir(exist_ok=True)
    report=dict(schema=1,checks=[],failures=[])
    oracle={}
    probes=[(name,delivery) for name,delivery in (
        ('point_indexed','materialized'),('int64_exact','materialized'),
        ('sum','materialized'),('null_count','materialized'),
        ('all_rows_wide','materialized'),('all_rows_wide','consume'))]
    for engine in ('sqlite','postgres','mysql','powdb-server','powdb-server-sql'):
        root=Path(tempfile.mkdtemp(prefix=engine+'-',dir=data))
        (root/'.competitive-disposable').touch()
        server=Server()
        worker=None
        try:
            image=pins.get('powdb' if engine.startswith('powdb-server') else engine)
            server.start(engine,root,None,image)
            binary=work/'bin'/binary_name(engine)
            setup=Worker(binary,engine,root,1000,'prepare',None,120)
            setup.close()
            worker=Worker(binary,engine,root,1000,'query',None,120)
            if engine=='postgres':
                for key,expected in [('max_parallel_workers_per_gather','0'),('fsync','on'),
                                     ('synchronous_commit','on'),('full_page_writes','on')]:
                    if worker.ready['metadata'].get(key)!=expected:
                        raise ValueError(f'PostgreSQL setting {key}: {worker.ready["metadata"]}')
            for name,delivery in probes:
                case=next(c for c in CASES if c['id']==name)
                query=query_for(case,engine,0,parameters(1000,0))
                result=worker.request([query],delivery)
                if 'error' in result:
                    raise ValueError(f'{name}: {result}')
                if len(result.get('samples',[]))!=1:
                    raise ValueError(f'{name}: missing smoke sample: {result}')
                sample=result['samples'][0]
                observed=(sample['digest'],sample['rows'],sample['preview'])
                if engine=='sqlite':
                    if name=='point_indexed' and sample['preview']!=[[17,'user_17',-483]]:
                        raise ValueError('SQLite seed point differs')
                    if name=='int64_exact' and sample['preview']!=[[9007199254740993]]:
                        raise ValueError('SQLite int64 loses precision')
                    if name=='sum' and sample['preview']!=[[-500]]:
                        raise ValueError('SQLite seed sum differs')
                    oracle[name,delivery]=observed
                elif observed!=oracle[name,delivery]:
                    raise ValueError(f'{name}/{delivery}: typed result differs: {sample["preview"]}')
                report['checks'].append(dict(engine=engine,case=name,delivery=delivery,pass_gate=True))
            if engine!='powdb-server-sql':
                result=worker.request(['bound point'],'materialized',[17])
                if len(result.get('samples',[]))!=1 or result['samples'][0].get('preview')!=[[17,'user_17',-483]]:
                    raise ValueError(f'bound point: {result}')
                report['checks'].append(dict(engine=engine,case='point_bound',pass_gate=True))
            print(json.dumps(dict(server_smoke=engine,status='pass')),flush=True)
        except (OSError,ValueError,RuntimeError,TimeoutError,subprocess.SubprocessError) as error:
            failure=dict(engine=engine,error=str(error))
            report['failures'].append(failure)
            print(json.dumps(dict(server_smoke_failure=failure)),flush=True)
        finally:
            if worker:worker.close(kill=True)
            server.close()
            shutil.rmtree(root)
            destination.write_text(json.dumps(report,indent=2)+'\n')
    return not report['failures']


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work',type=Path,required=True)
    parser.add_argument('--pins',type=Path,required=True)
    args=parser.parse_args()
    return 0 if smoke(args.work.resolve(),json.loads(args.pins.read_text())['images']) else 1


if __name__=='__main__':raise SystemExit(main())
