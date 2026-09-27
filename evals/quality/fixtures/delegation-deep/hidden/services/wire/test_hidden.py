"""Hidden checks for services/wire (package frames): SPEC examples plus randomized comparison with a reference."""
import random
import unittest
import zlib

from frames import (
    HEADER,
    BadHeader,
    CorruptFrame,
    DecodeError,
    EncodeError,
    Schema,
    StreamDecoder,
    StreamError,
    TruncatedError,
    TruncatedStream,
    decode_deltas,
    decode_record,
    decode_stream,
    decode_svarint,
    decode_uvarint,
    encode_deltas,
    encode_frame,
    encode_record,
    encode_stream,
    encode_svarint,
    encode_uvarint,
    unzigzag,
    zigzag,
)

LO, HI = -(1 << 63), (1 << 63) - 1


def ref_uvarint(n):
    out = []
    while True:
        low = n % 128
        n //= 128
        if n:
            out.append(low + 128)
        else:
            out.append(low)
            return bytes(out)


def ref_zigzag(n):
    return 2 * n if n >= 0 else -2 * n - 1


def ref_deltas(values):
    out = ref_uvarint(len(values))
    previous = 0
    for value in values:
        out += ref_uvarint(ref_zigzag(value - previous))
        previous = value
    return out


def ref_frame(frame_type, payload):
    return bytes([frame_type]) + ref_uvarint(len(payload)) + payload + zlib.crc32(bytes([frame_type]) + payload).to_bytes(4, "big")


class WireHidden(unittest.TestCase):
    def test_spec_examples(self):
        self.assertEqual(encode_uvarint(0), b"\x00")
        self.assertEqual(encode_uvarint(300), b"\xac\x02")
        self.assertEqual(encode_svarint(-1), b"\x01")
        self.assertEqual(encode_svarint(64), b"\x80\x01")
        self.assertEqual(encode_deltas([100, 98, 98]), b"\x03\xc8\x01\x03\x00")
        self.assertEqual(encode_deltas([]), b"\x00")
        schema = Schema({1: "uint", 2: "str", 3: "sints"})
        data = encode_record(schema, {1: 150, 2: "hi", 3: [1, -1]})
        self.assertEqual(data, b"\x08\x96\x01\x12\x02hi\x1a\x03\x02\x02\x03")
        self.assertEqual(decode_record(schema, data), {1: 150, 2: "hi", 3: [1, -1]})
        self.assertEqual(encode_frame(1, b""), b"\x01\x00\xa5\x05\xdf\x1b")

    def test_encoding_is_repeatable(self):
        schema = Schema({1: "uint", 2: "sint", 3: "bytes", 5: "str"})
        rnd = random.Random(7005)
        for _ in range(200):
            record = {1: rnd.randint(0, 300), 2: rnd.randint(-300, 300), 3: bytes(rnd.randrange(256) for _ in range(rnd.randint(0, 9))), 5: "ab"}
            expected = (
                b"\x08" + ref_uvarint(record[1]) + b"\x10" + ref_uvarint(ref_zigzag(record[2]))
                + b"\x1a" + ref_uvarint(len(record[3])) + record[3] + b"\x2a\x02ab"
            )
            self.assertEqual(encode_record(schema, record), expected, record)
            self.assertEqual(decode_record(schema, encode_record(schema, record)), record)
        for n in (0, 8, 16, 26, 42, 127, 128, 300):
            self.assertEqual(encode_uvarint(n), ref_uvarint(n), n)

    def test_series_changed_in_place(self):
        schema = Schema({4: "sints"})
        rnd = random.Random(7006)
        series = [1, 2, 3]
        for _ in range(100):
            self.assertEqual(encode_record(schema, {4: series}), b"\x22" + ref_uvarint(len(ref_deltas(series))) + ref_deltas(series))
            self.assertEqual(decode_record(schema, encode_record(schema, {4: series})), {4: series})
            series[rnd.randrange(len(series))] = rnd.randint(-1000, 1000)
            if rnd.random() < 0.3:
                series.append(rnd.randint(-5, 5))

    def test_varint_errors(self):
        for bad in (-1, 1 << 70, True, 1.0):
            with self.assertRaises(EncodeError):
                encode_uvarint(bad)
        self.assertEqual(decode_uvarint(ref_uvarint((1 << 70) - 1)), ((1 << 70) - 1, 10))
        with self.assertRaises(TruncatedError):
            decode_uvarint(b"\x80\x80")
        with self.assertRaises(DecodeError):
            decode_uvarint(b"\x80" * 10 + b"\x01")
        with self.assertRaises(DecodeError):
            decode_uvarint(b"\x81\x00")
        for bad in (LO - 1, HI + 1):
            with self.assertRaises(EncodeError):
                encode_svarint(bad)
        with self.assertRaises(DecodeError):
            decode_svarint(ref_uvarint(ref_zigzag(HI + 1)))

    def test_signed_extremes(self):
        schema = Schema({2: "sint"})
        for value in (LO, LO + 1, -1, 0, HI):
            self.assertEqual(encode_svarint(value), ref_uvarint(ref_zigzag(value)))
            data = encode_record(schema, {2: value})
            self.assertEqual(data, b"\x10" + ref_uvarint(ref_zigzag(value)), value)
            self.assertEqual(decode_record(schema, data), {2: value})
        for bad in (LO - 1, HI + 1):
            with self.assertRaises(EncodeError):
                encode_record(schema, {2: bad})

    def test_zigzag_any_size(self):
        rnd = random.Random(7001)
        for _ in range(500):
            n = rnd.randint(-(1 << rnd.randint(1, 90)), 1 << rnd.randint(1, 90))
            self.assertEqual(zigzag(n), ref_zigzag(n), n)
            self.assertEqual(unzigzag(ref_zigzag(n)), n, n)

    def test_packed_series_against_reference(self):
        rnd = random.Random(7002)
        pool = [LO, HI, LO + 1, HI - 1, -1, 0, 1]
        for _ in range(400):
            values = [rnd.choice(pool) if rnd.random() < 0.4 else rnd.randint(LO, HI) for _ in range(rnd.randint(0, 8))]
            data = encode_deltas(values)
            self.assertEqual(data, ref_deltas(values), values)
            self.assertEqual(decode_deltas(data), values)
        self.assertEqual(decode_deltas(b"\x00\x02\x02\x03", 1, 4), [1, -1])
        with self.assertRaises(DecodeError):
            decode_deltas(b"\x01\x02\x02")
        with self.assertRaises(TruncatedError):
            decode_deltas(b"\x03\x02")
        with self.assertRaises(DecodeError):
            decode_deltas(ref_uvarint(1) + ref_uvarint(ref_zigzag(HI + 1)))

    def test_records_against_reference(self):
        rnd = random.Random(7003)
        kinds = ["uint", "sint", "bytes", "str", "sints"]
        for _ in range(300):
            fields = {fid: rnd.choice(kinds) for fid in rnd.sample(range(1, 1 << 20), rnd.randint(1, 5))}
            values, expected = {}, b""
            for fid in sorted(fields):
                kind = fields[fid]
                if kind == "uint":
                    value = rnd.choice([0, (1 << 64) - 1, rnd.randint(0, 1 << 64)]) % (1 << 64)
                    body, wt = ref_uvarint(value), 0
                elif kind == "sint":
                    value = rnd.choice([LO, HI, rnd.randint(LO, HI)])
                    body, wt = ref_uvarint(ref_zigzag(value)), 0
                elif kind == "bytes":
                    value = bytes(rnd.randrange(256) for _ in range(rnd.randint(0, 140)))
                    body, wt = ref_uvarint(len(value)) + value, 2
                elif kind == "str":
                    value = "".join(rnd.choice("xyé😀") for _ in range(rnd.randint(0, 30)))
                    raw = value.encode()
                    body, wt = ref_uvarint(len(raw)) + raw, 2
                else:
                    value = [rnd.choice([LO, HI, rnd.randint(-99, 99)]) for _ in range(rnd.randint(0, 6))]
                    raw = ref_deltas(value)
                    body, wt = ref_uvarint(len(raw)) + raw, 2
                values[fid] = value
                expected += ref_uvarint((fid << 3) | wt) + body
            schema = Schema(fields)
            self.assertEqual(encode_record(schema, values), expected, (fields, values))
            self.assertEqual(decode_record(schema, expected), values)

    def test_record_rules(self):
        schema = Schema({1: "uint", 2: "str"})
        self.assertEqual(encode_record(schema, {1: None, 2: "a"}), b"\x12\x01a")
        self.assertEqual(decode_record(schema, b"\x08\x01\x08\x02"), {1: 2})
        self.assertEqual(decode_record(schema, b"\x18\x05\x22\x01z\x08\x07"), {1: 7})
        for bad in (b"\x0a\x00", b"\x00\x01", b"\x1b", b"\x12\x01\xff", b"\x08" + ref_uvarint(1 << 64)):
            with self.assertRaises(DecodeError, msg=bad):
                decode_record(schema, bad)
        with self.assertRaises(TruncatedError):
            decode_record(schema, b"\x12\x05ab")
        with self.assertRaises(EncodeError):
            encode_record(schema, {3: 1})
        with self.assertRaises(EncodeError):
            encode_record(schema, {1: -1})

    def test_stream_errors(self):
        stream = encode_stream([(1, b"abc"), (2, b"")])
        self.assertEqual(stream, HEADER + ref_frame(1, b"abc") + ref_frame(2, b""))
        with self.assertRaises(BadHeader):
            StreamDecoder().feed(b"WX")
        with self.assertRaises(TruncatedStream):
            decode_stream(b"")
        with self.assertRaises(TruncatedStream):
            decode_stream(stream[:-1])
        with self.assertRaises(CorruptFrame):
            decode_stream(HEADER + b"\x00\x00" + b"\x00" * 4)
        with self.assertRaises(CorruptFrame):
            decode_stream(HEADER + b"\x01" + ref_uvarint(65537))
        decoder = StreamDecoder()
        with self.assertRaises(CorruptFrame):
            decoder.feed(HEADER + b"\x01\x80\x00")
        with self.assertRaises(StreamError):
            decoder.feed(b"")
        decoder = StreamDecoder()
        decoder.feed(stream)
        decoder.close()
        with self.assertRaises(StreamError):
            decoder.feed(b"")
        damaged = bytearray(stream)
        damaged[len(HEADER) + 3] ^= 0x40
        decoder = StreamDecoder()
        self.assertEqual(decoder.feed(bytes(damaged)), [(2, b"")])
        self.assertEqual((decoder.dropped, decoder.delivered), (1, 1))
        for bad in (0, 256, True):
            with self.assertRaises(EncodeError):
                encode_frame(bad, b"")
        with self.assertRaises(EncodeError):
            encode_frame(1, b"x" * 65537)

    def test_large_streams_any_split(self):
        rnd = random.Random(7004)
        for _ in range(25):
            frames = [(rnd.randint(1, 255), bytes(rnd.randrange(256) for _ in range(rnd.choice([0, 3, rnd.randint(0, 900)])))) for _ in range(rnd.randint(15, 45))]
            stream = b"".join([HEADER] + [ref_frame(t, p) for t, p in frames])
            self.assertEqual(encode_stream(frames), stream)
            for largest in (5, 300, 2000):
                decoder, got, pos = StreamDecoder(), [], 0
                while pos < len(stream):
                    size = rnd.randint(0, largest)
                    got.extend(decoder.feed(stream[pos : pos + size]))
                    pos += size
                decoder.close()
                self.assertEqual([tuple(f) for f in got], frames, (len(stream), largest))
                self.assertEqual(decoder.dropped, 0)


if __name__ == "__main__":
    unittest.main()
