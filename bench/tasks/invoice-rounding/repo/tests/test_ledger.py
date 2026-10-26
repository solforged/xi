import contextlib
import io
import itertools
import json
import tempfile
import unittest
from decimal import Decimal, localcontext, ROUND_UP
from pathlib import Path
from ledger.model import Options
from ledger.config import load, override
from ledger.parser import parse
from ledger.aggregate import totals
from ledger.output import render
from ledger.cli import main

class Ledger(unittest.TestCase):
    def test_defaults_and_validation(self):
        self.assertEqual((load({}).precision, load({}).rounding, load({}).scope), (2, 'even', 'line'))
        for bad in ({'precision': True}, {'precision': -1}, {'precision': 7}, {'precision': 2.0}, {'scope':'batch'}, {'rounding':'up'}, {'typo':1}):
            with self.subTest(bad=bad), self.assertRaises(ValueError): load(bad)
        mapping = {'precision': 3, 'rounding':'away', 'scope':'invoice'}
        opt = load(mapping)
        new = override(opt, precision=0, rounding='even')
        self.assertEqual((new.precision,new.rounding,new.scope),(0,'even','invoice'))
        self.assertEqual(opt.precision,3)
        self.assertEqual(mapping, {'precision':3,'rounding':'away','scope':'invoice'})
        with self.assertRaises(ValueError): override(opt, precision=False)

    def test_parser(self):
        rows = list(parse(iter(['  \n','"inv,42", USD , 1.25\n','x,EUR,-2\n'])))
        self.assertEqual([(r.invoice,r.currency,r.amount) for r in rows],[('inv,42','USD',Decimal('1.25')),('x','EUR',Decimal('-2'))])
        for row in ('x,usd,1','x,US1,2','x,USD,NaN','x,USD,Infinity','x,USD,no','x,USD','x,USD,1,2',',USD,1','x,,1','"x,USD,1'):
            with self.subTest(row=row), self.assertRaisesRegex(ValueError, '2'):
                list(parse(iter(['\n',row])))

    def test_scope_currency_and_order(self):
        lines = ['a,USD,0.005','a,USD,0.005','b,USD,0.005','a,EUR,0.005']
        for order in itertools.permutations(lines):
            for rounding, scope, expected in [('even','line',{'EUR':'0.00','USD':'0.00'}),('away','line',{'EUR':'0.01','USD':'0.03'}),('even','invoice',{'EUR':'0.00','USD':'0.01'}),('away','invoice',{'EUR':'0.01','USD':'0.02'})]:
                opt=load({'rounding':rounding,'scope':scope})
                self.assertEqual(json.loads(render(totals(parse(iter(order)),opt),opt)),expected)

    def test_negative_ties_and_zero(self):
        for rounding, expected in [('even','-0.02'),('away','-0.03')]:
            opt=load({'rounding':rounding})
            self.assertEqual(json.loads(render(totals(parse(['a,USD,-0.005','b,USD,-0.015']),opt),opt)),{'USD':expected})
        opt=load({})
        self.assertEqual(render({'USD':Decimal('-0.0001')},opt),'{"USD": "0.00"}')
        self.assertEqual(render({},opt),'{}')
        self.assertLess(render({'USD':Decimal(1),'EUR':Decimal(2)},opt).index('EUR'),render({'USD':Decimal(1),'EUR':Decimal(2)},opt).index('USD'))

    def test_context_independence(self):
        amount = '123456789012345678901234567890.125'
        with localcontext() as ctx:
            ctx.prec=3
            ctx.rounding=ROUND_UP
            opt=load({'scope':'invoice'})
            result=json.loads(render(totals(parse(['a,USD,'+amount,'a,USD,0.005','b,USD,-0.005']),opt),opt))
            self.assertEqual(result,{'USD':'123456789012345678901234567890.13'})
            self.assertEqual((ctx.prec,ctx.rounding),(3,ROUND_UP))

    def test_cli(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            (root/'input.csv').write_text('x,USD,2.5\nx,USD,2.5\ny,EUR,0.5\n')
            (root/'config.json').write_text(json.dumps({'precision':2,'rounding':'away','scope':'line'}))
            out=io.StringIO()
            with contextlib.redirect_stdout(out):
                main([str(root/'input.csv'),'--config',str(root/'config.json'),'--precision','0','--scope','invoice','--rounding','even'])
            self.assertEqual(json.loads(out.getvalue()),{'EUR':'0','USD':'5'})
