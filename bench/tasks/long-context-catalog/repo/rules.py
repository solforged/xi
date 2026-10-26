import json
from pathlib import Path

CATALOG = json.loads(Path(__file__).with_name('catalog.json').read_text())

def policy(region, operation):
    return CATALOG.get(region, {}).get(operation, {})
