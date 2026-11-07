"""Socket creation must not be mistaken for a ready database server."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from run import Server


class Readiness(unittest.TestCase):
    def check_engine(self,engine):
        server=Server()
        probes=[]
        def command(args,**kwargs):
            if args[:2]==['docker','inspect']:
                return json.dumps({'Running':True,'Pid':12345})
            return 'owned-container-id'
        def probe(args,**kwargs):
            if args[:2]==['docker','exec']:
                self.assertIsNone(server.pid,'server became ready before probe acceptance')
                probes.append(args)
                return subprocess.CompletedProcess(args,1 if len(probes)==1 else 0)
            return subprocess.CompletedProcess(args,0)
        with tempfile.TemporaryDirectory() as temp:
            with patch('run.subprocess.check_output',side_effect=command), \
                 patch('run.subprocess.run',side_effect=probe), \
                 patch('run.Path.exists',return_value=True), \
                 patch('run.Path.read_text',return_value='postgres' if engine=='postgres' else 'mysqld'), \
                 patch('run.time.sleep'):
                server.start(engine,Path(temp),None,'fixture@sha256:'+'a'*64)
                self.assertEqual(len(probes),2,'first refusing server probe was ignored')
                self.assertEqual(server.pid,12345)
                server.close()

    def test_postgres_waits_for_accepting_server(self):self.check_engine('postgres')
    def test_mysql_waits_for_accepting_server(self):self.check_engine('mysql')


if __name__=='__main__':unittest.main()
