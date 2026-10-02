"""Minimal decoding for local verification of Alloy's Snappy/protobuf requests."""


def varint(data, at=0):
    value = shift = 0
    while True:
        b = data[at]
        at += 1
        value |= (b & 127) << shift
        if b < 128:
            return value, at
        shift += 7
        if shift > 64:
            raise ValueError("Invalid protobuf varint")


def snappy(data):
    size, at = varint(data)
    if size > 8 * 1024 * 1024:
        raise ValueError("Oversize test payload")
    out = bytearray()
    while at < len(data):
        tag = data[at]
        at += 1
        kind = tag & 3
        if kind == 0:
            n = tag >> 2
            if n >= 60:
                width = n - 59
                n = int.from_bytes(data[at:at + width], "little")
                at += width
            out.extend(data[at:at + n + 1])
            at += n + 1
        else:
            width = {1: 1, 2: 2, 3: 4}[kind]
            length = 4 + ((tag >> 2) & 7) if kind == 1 else 1 + (tag >> 2)
            offset = int.from_bytes(data[at:at + width], "little")
            if kind == 1:
                offset += (tag & 224) << 3
            at += width
            if offset <= 0 or offset > len(out):
                raise ValueError("Invalid Snappy offset")
            for _ in range(length):
                out.append(out[-offset])
        if len(out) > size:
            raise ValueError("Invalid Snappy length")
    if len(out) != size:
        raise ValueError("Truncated Snappy payload")
    return bytes(out)


def fields(data):
    at = 0
    while at < len(data):
        tag, at = varint(data, at)
        wire = tag & 7
        if wire == 0:
            value, at = varint(data, at)
        elif wire == 2:
            length, at = varint(data, at)
            value = data[at:at + length]
            at += length
        elif wire in (1, 5):
            length = 8 if wire == 1 else 4
            value = data[at:at + length]
            at += length
        else:
            raise ValueError("Unsupported protobuf wire type")
        yield tag >> 3, value


def log_entries(data):
    for number, stream in fields(data):
        if number != 1:
            continue
        parts = list(fields(stream))
        labels = next(value.decode() for number, value in parts if number == 1)
        for number, entry in parts:
            if number != 2:
                continue
            body = dict(fields(entry))
            yield labels, body.get(2, b"").decode("utf8", "replace")
