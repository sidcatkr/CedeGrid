"""Real loopback mTLS and deadline tests; no mock successful network transport."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import os
import ssl
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from cedegrid import Client, RemoteError, DeadlineExceeded, parse_json, stringify_json

class TransportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary=tempfile.TemporaryDirectory()
        cls.root=Path(cls.temporary.name)
        def run(*args):subprocess.run(['openssl',*args],cwd=cls.root,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for ca in ('ca','wrong'):
            run('req','-x509','-newkey','rsa:2048','-nodes','-keyout',ca+'.key','-out',ca+'.pem','-subj','/CN='+ca,'-days','1','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign')
        for name in ('server','operator','unauthorized'):
            run('req','-newkey','rsa:2048','-nodes','-keyout',name+'.key','-out',name+'.csr','-subj','/CN='+name)
            (cls.root/(name+'.ext')).write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nsubjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' if name=='server' else 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n')
            run('x509','-req','-in',name+'.csr','-CA','ca.pem','-CAkey','ca.key','-CAcreateserial','-out',name+'.pem','-days','1','-extfile',name+'.ext')
        cls.requests=0
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_POST(self):
                cls.requests+=1
                subject=dict(item for section in self.connection.getpeercert()['subject'] for item in section)
                if subject.get('commonName')!='operator':self.send_response(403);self.end_headers();self.wfile.write(b'{}');return
                request=parse_json(self.rfile.read(int(self.headers['Content-Length'])))
                if request['op']=='redirect':self.send_response(302);self.send_header('Location','/v1/rpc');self.end_headers();return
                self.send_response(200);self.end_headers()
                try:
                    if request['op']=='slow':self.wfile.write(b'{"kind":"');self.wfile.flush();time.sleep(.25);self.wfile.write(b'ok"}');return
                    if request['op']=='large':self.wfile.write(b'x'*(3*1024*1024+1));return
                    self.wfile.write(stringify_json({'kind':'echo','body':request}).encode())
                except (BrokenPipeError,ConnectionResetError,ssl.SSLError):pass
        cls.server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cls.root/'server.pem',cls.root/'server.key')
        context.load_verify_locations(cls.root/'ca.pem');context.verify_mode=ssl.CERT_REQUIRED
        cls.server.socket=context.wrap_socket(cls.server.socket,server_side=True)
        cls.thread=threading.Thread(target=cls.server.serve_forever,daemon=True);cls.thread.start()
        cls.endpoint='https://127.0.0.1:'+str(cls.server.server_port)
    @classmethod
    def tearDownClass(cls):cls.server.shutdown();cls.server.server_close();cls.thread.join();cls.temporary.cleanup()
    def client(self,name='operator',**options):
        options={'ca':self.root/'ca.pem','certificate':self.root/(name+'.pem'),'private_key':self.root/(name+'.key'),'max_transfer_bytes_per_second':0,**options}
        return Client(self.endpoint,**options)
    def test_exact_numeric_transport_and_environment_proxy_bypass(self):
        with patch.dict(os.environ,{'HTTPS_PROXY':'http://127.0.0.1:1','HTTP_PROXY':'http://127.0.0.1:1','ALL_PROXY':'http://127.0.0.1:1'}):
            result=self.client().request('echo',generation=2**53+1,metadata={'integer':2**64-1,'float':1.0,'zero':-0.0})['body']
        self.assertEqual(result['generation'],2**53+1);self.assertEqual(result['metadata']['integer'],2**64-1)
        self.assertIs(type(result['metadata']['float']),float)
        self.assertIn('-0.0',stringify_json(result))
    def test_wrong_ca_wrong_hostname_and_unauthorized_leaf(self):
        with self.assertRaises(RemoteError):self.client(ca=self.root/'wrong.pem').request('echo')
        original=self.endpoint;self.endpoint=original.replace('127.0.0.1','localhost')
        try:
            with self.assertRaises(RemoteError) as caught:self.client().request('echo')
            self.assertIsInstance(caught.exception.cause,ssl.SSLCertVerificationError)
        finally:self.endpoint=original
        with self.assertRaises(RemoteError) as caught:self.client('unauthorized').request('echo')
        self.assertEqual(caught.exception.code,'ERR_CEDEGRID_HTTP')
        missing=self.client();missing.context=ssl.create_default_context(cafile=str(self.root/'ca.pem'))
        with self.assertRaises(RemoteError):missing.request('echo')
    def test_redirect_is_not_followed_and_responses_are_bounded(self):
        before=self.requests
        with self.assertRaises(RemoteError):self.client().request('redirect')
        self.assertEqual(self.requests,before+1)
        with self.assertRaises(RemoteError) as caught:self.client().request('large')
        self.assertEqual(caught.exception.code,'ERR_CEDEGRID_RESPONSE_TOO_LARGE')
    def test_end_to_end_body_pacing_and_queue_deadlines(self):
        slow=self.client(timeout=.05);start=time.monotonic()
        with self.assertRaises(DeadlineExceeded):slow.request('slow')
        self.assertLess(time.monotonic()-start,.2)
        paced=self.client(timeout=.02,max_transfer_bytes_per_second=1)
        before=self.requests
        with self.assertRaises(DeadlineExceeded):paced.request('echo')
        self.assertEqual(self.requests,before)
        queued=self.client(timeout=.02);queued._request_lock.acquire()
        try:
            with self.assertRaises(DeadlineExceeded):queued.request('echo')
        finally:queued._request_lock.release()
    def test_dns_cannot_transmit_after_deadline(self):
        from cedegrid import transport
        lookup=transport.socket.getaddrinfo
        def slow_lookup(*args,**kwargs):time.sleep(.1);return lookup(*args,**kwargs)
        client=self.client(timeout=.02);before=self.requests;start=time.monotonic()
        with patch.object(transport.socket,'getaddrinfo',side_effect=slow_lookup):
            with self.assertRaises(DeadlineExceeded):client.request('echo')
        self.assertLess(time.monotonic()-start,.09);time.sleep(.12);self.assertEqual(self.requests,before)

if __name__=='__main__':unittest.main()
