def resolve(document,pointer):
    value=document
    for token in pointer.strip('/').split('/'):
        value=value[int(token)] if isinstance(value,list) else value[token]
    return value
