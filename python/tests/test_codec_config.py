import math
from pathlib import Path
import tempfile
import unittest
from cedegrid import parse_json, stringify_json, load_client_config, ConfigError, ValidationError
from cedegrid.config import endpoint_origin
from cedegrid.codec import integer, I64_MAX, U64_MAX

class CodecTests(unittest.TestCase):
    def test_exact_integer_and_float_kinds(self):
        value = parse_json('{"nested":[9007199254740993,18446744073709551615,-9223372036854775808,1,1.0,-0,-0.0,5e-324,1.7976931348623157e308],"语言":"한국어"}')
        items=value['nested']
        self.assertEqual(items[:3],[2**53+1,2**64-1,-2**63])
        self.assertIs(type(items[3]),int);self.assertIs(type(items[4]),float)
        self.assertEqual(math.copysign(1,items[5]),-1)
        self.assertEqual(math.copysign(1,items[6]),-1)
        self.assertEqual(parse_json(stringify_json(value)),value)
        self.assertIn('1.0',stringify_json(value));self.assertIn('-0.0',stringify_json(value))
    def test_rejected_tokens_and_values(self):
        for text in ['18446744073709551616','-9223372036854775809','1e309','1e-999','NaN','Infinity','{"a":1,"a":2}','"\\ud800"']:
            with self.subTest(text=text),self.assertRaises(ValidationError):parse_json(text)
        for value in [float('nan'),float('inf'),2**64,-2**63-1,{1:'bad'}]:
            with self.subTest(value=value),self.assertRaises(ValidationError):stringify_json(value)
        self.assertEqual(parse_json('0e-999'),0.0)
    def test_integer_fields_are_separate_from_metadata(self):
        self.assertEqual(integer(U64_MAX),U64_MAX)
        for value in [True,1.0,-1,2**64]:
            with self.assertRaises(ValidationError):integer(value)
        with self.assertRaises(ValidationError):integer(2**63,maximum=I64_MAX)
    def test_prototype_like_keys_remain_plain_metadata(self):
        raw='{"__proto__":{"polluted":true},"constructor":1,"$cedegrid$x":2}'
        self.assertEqual(parse_json(stringify_json(parse_json(raw))),parse_json(raw))

class ConfigTests(unittest.TestCase):
    def test_origins(self):
        for origin in ['https://example.test','https://example.test/','https://[::1]:9443','https://localhost:65535']:
            self.assertEqual(endpoint_origin(origin),origin.rstrip('/'))
        for origin in ['http://example.test','https://example.test?','https://example.test#','https://@example.test','https://user@example.test','https://example.test/path','https://example.test:0','https://example.test:65536','https://example.test:','https://example.test\\path','https://example.test\n']:
            with self.subTest(origin=origin),self.assertRaises(ConfigError):endpoint_origin(origin)
    def test_toml_scalar_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'client.toml'
            def load(fields):
                path.write_text('config_version = 1\nendpoint = "https://example.test"\n'+fields+'\n[tls]\nca_cert="tls/ca.pem"\ncertificate="tls/client.pem"\nprivate_key="tls/key.pem"\n')
                return load_client_config(path)
            self.assertEqual(load('')['timeout'],15)
            self.assertEqual(load('max_transfer_bytes_per_second=9007199254740993')['max_transfer_bytes_per_second'],9007199254740993)
            self.assertEqual(load('max_transfer_bytes_per_second=0')['max_transfer_bytes_per_second'],0)
            for fields in ['config_version=1','unexpected=true','max_transfer_bytes_per_second=9223372036854775808','max_transfer_bytes_per_second=1.0','timeout_seconds=nan','timeout_seconds=inf','timeout_seconds=true','timeout_seconds=1979-05-27','timeout_seconds=-1']:
                with self.subTest(fields=fields),self.assertRaises(ConfigError):load(fields)
            path.write_text('{"endpoint":"https://example.test"}')
            with self.assertRaises(ConfigError):load_client_config(path)

if __name__=='__main__':unittest.main()
