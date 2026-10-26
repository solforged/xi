def include(document):
    return not document["archived"] and not document["deleted"]
