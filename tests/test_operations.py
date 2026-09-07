import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'tools'))
from validation_runtime import OwnedProcess, inside_home
import soak

class Client:
    def __init__(self): self.drains=[]
    def status(self): return {'kind':'status','tasks':[]}
    def drain_node(self,node,drain=True): self.drains.append((node,drain))

class Operations(unittest.TestCase):
    def test_outside_home_rejected(self):
        with self.assertRaises(ValueError): inside_home('/var/tmp/resmgr-not-allowed')
    def test_soak_window_required(self):
        with self.assertRaises(ValueError): soak.validate({'duration_seconds':86400})
    def test_owned_cleanup_does_not_signal_unrelated_child(self):
        with tempfile.TemporaryDirectory() as root:
            a=OwnedProcess([sys.executable,'-c','import time;time.sleep(30)'],root,Path(root)/'a.log')
            b=OwnedProcess([sys.executable,'-c','import time;time.sleep(30)'],root,Path(root)/'b.log')
            try:
                self.assertTrue(a.stop()['reaped'])
                self.assertIsNone(b.poll())
            finally:b.stop()
    def test_short_harness_is_never_soak_success(self):
        with tempfile.TemporaryDirectory() as root:
            c=Client()
            r=soak.run({'duration_seconds':.02,'execution_approved':True,'two_node_window_approved':True,
                'isolated_validation_deployment':True,'node_ids':['a','b'],'anchor_node_id':'a',
                'experiment_id':'test','task_prefix':'test.','job_prefix':'test.','connection':str(Path(root)/'client.json'),
                'output':str(Path(root)/'case'),'sample_seconds':.01,'events':[]},client=c)
            self.assertFalse(r['actual_24_hour_soak'])
            self.assertEqual(r['operational_acceptance'],'pending_review')
            self.assertEqual(c.drains,[('a',True),('b',True)])

if __name__=='__main__':unittest.main()
