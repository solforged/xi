import argparse
import json
from .config import load, override
from .parser import parse
from .aggregate import totals
from .output import render

def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument('input')
    parser.add_argument('--config')
    parser.add_argument('--precision', type=int)
    args = parser.parse_args(argv)
    mapping = {}
    if args.config:
        with open(args.config) as stream:
            mapping = json.load(stream)
    options = override(load(mapping), precision=args.precision)
    with open(args.input) as stream:
        result = totals(parse(stream), options)
    print(render(result, options))

if __name__ == '__main__':
    main()
