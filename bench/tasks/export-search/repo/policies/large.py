def include(document):
    return document["size"] >= 100 and not document["deleted"]
