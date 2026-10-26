class Decoder:
    def __init__(self): self.buffer=''
    def feed(self, chunk, final=False):
        self.buffer+=chunk
        parts=self.buffer.split('\n')
        self.buffer=parts.pop()
        if final and self.buffer: parts.append(self.buffer)
        return [row.split(',') for row in parts]
