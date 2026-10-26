import unittest
from response_cache import cache_key


class CacheTests(unittest.TestCase):
    def test_host_casing(self):
        self.assertEqual(cache_key("get", "HTTPS://API.Example/a"), cache_key("GET", "https://api.example/a"))

    def test_path_and_query_are_case_sensitive(self):
        self.assertNotEqual(cache_key("GET", "https://x/A?q=X"), cache_key("GET", "https://x/a?q=X"))
        self.assertNotEqual(cache_key("GET", "https://x/a?q=X"), cache_key("GET", "https://x/a?q=x"))

    def test_user_info_preserved(self):
        self.assertEqual(cache_key("GET", "https://User:Pass@HOST:8443/A"), ("GET", "https://User:Pass@host:8443/A"))
        self.assertNotEqual(cache_key("GET", "https://User:Pass@host/A"), cache_key("GET", "https://user:pass@host/A"))

    def test_ports_fragments_and_methods(self):
        self.assertEqual(cache_key("get", "https://HOST:443/a#one"), cache_key("GET", "https://host:443/a#two"))
        self.assertNotEqual(cache_key("GET", "https://host:443/a"), cache_key("GET", "https://host/a"))
        self.assertNotEqual(cache_key("GET", "https://host/a"), cache_key("POST", "https://host/a"))

    def test_ipv6(self):
        self.assertEqual(cache_key("get", "http://[ABCD::1]:8080/A"), ("GET", "http://[abcd::1]:8080/A"))
