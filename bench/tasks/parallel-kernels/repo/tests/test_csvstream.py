import unittest
from packages.csvstream import Decoder
class Csv(unittest.TestCase):
    def test_every_two_chunk_split(self):
        text='a,"b,c","d""e"\r\n"x\ny",,z\nlast,'
        expected=[['a','b,c','d"e'],['x\ny','','z'],['last','']]
        for split in range(len(text)+1):
            d=Decoder()
            self.assertEqual(d.feed(text[:split])+d.feed(text[split:],final=True),expected)
        d=Decoder(); rows=[]
        for c in text: rows+=d.feed(c)
        self.assertEqual(rows+d.feed('',True),expected)
    def test_empty_and_final(self):
        for text,expected in [('',[]),('\n',[['']]),(',', [['','']]),('""',[['']]),('a\n',[['a']])]:
            d=Decoder(); self.assertEqual(d.feed(text,True),expected)
            with self.assertRaises(ValueError): d.feed('')
    def test_invalid(self):
        for text in ['"open','a"b','"a"z','a\r','a\rb']:
            for split in range(len(text)+1):
                d=Decoder()
                with self.assertRaises(ValueError): d.feed(text[:split]); d.feed(text[split:],True)
