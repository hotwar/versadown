#!/usr/bin/env python3
"""Generates src/cloud/mega/account/manage_vectors.rs with an implementation
that is independent from the Rust code under test (Python standard library +
`cryptography`).

Algorithms (written from the public megajs 1.3.x reference, lib/crypto/*,
lib/storage.mjs and lib/mutable-file.mjs, and from the behaviour of the MEGA
open source SDK; nothing is copied from either):

* file key (32 bytes) - k[0..16] = aes_key XOR k[16..32], k[16..24] = nonce,
  k[24..32] = meta_mac. The node key is wrapped for the account as
  AES-128-ECB(master key, key). Folder keys are plain 16 byte AES keys.
* upload data   - AES-128-CTR with IV = nonce || 8 zero bytes and a 128 bit
  big endian counter, so any byte offset can be sought to (the counter is
  offset // 16, the first block is then skipped by offset % 16).
* meta_mac      - "condensed MAC": the plaintext is cut into chunks of 128 KiB,
  growing by 128 KiB per chunk up to a 1 MiB cap. Every chunk is CBC-MACed
  (IV = nonce || nonce, last block zero padded, the MAC is the last CBC
  block). The chunk MACs are chained through a zero-IV CBC (acc = ECB(acc XOR
  chunk_mac)) and the 16 byte result is folded to 8 bytes:
  folded[i] = c[i] ^ c[i+4], folded[i+4] = c[i+8] ^ c[i+12] for i < 4. A file
  ending exactly on a chunk boundary has no trailing empty chunk; an empty
  file keeps the all-zero accumulator.
* attributes    - AES-128-CBC (zero IV) of zero padded `MEGA` + compact JSON
  (sorted keys, ensure_ascii=False). Folders use their key, files the key's
  first half XOR second half.
* folder export - the share key `ok` is AES-ECB(master, share key), `ha` is
  AES-ECB(master, handle || handle), and the `cr` field of the share-update
  command is `[[share handle], [node handles...], [0, node index,
  b64(AES-ECB(share key, node key)), ...]]`.
* incoming share - `privk` is AES-ECB(master) of the RSA MPIs p, q, d, u (2
  byte big endian bit length + magnitude). The share key arrives as `sk`, an
  MPI holding m^e mod n where m starts with the 16 byte share key and is
  padded with random bytes to the size of a session id plaintext (256 bytes).
  The shared root node's `k` is "<root handle>:<b64 AES-ECB(share key, node
  key)>"; own nodes use "<user handle>:<b64 AES-ECB(master, node key)>".

Usage: python3 -I generate_manage_vectors.py > ../../../src/cloud/mega/account/manage_vectors.rs
The RSA key pair (and the random padding of `sk`) is generated on each run, so
regenerating changes the (committed) output; the Rust tests only depend on
internal consistency. A self check re-decrypts everything before printing.
"""
import base64
import hashlib
import json
import secrets
import sys

from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def unb64(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def xor(a: bytes, b: bytes) -> bytes:
    return bytes(x ^ y for x, y in zip(a, b))


def ecb_enc(key, data):
    e = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return e.update(data) + e.finalize()


def ecb_dec(key, data):
    d = Cipher(algorithms.AES(key), modes.ECB()).decryptor()
    return d.update(data) + d.finalize()


def cbc_enc_zero_iv(key, data):
    e = Cipher(algorithms.AES(key), modes.CBC(bytes(16))).encryptor()
    return e.update(data) + e.finalize()


def cbc_dec_zero_iv(key, data):
    d = Cipher(algorithms.AES(key), modes.CBC(bytes(16))).decryptor()
    return d.update(data) + d.finalize()


def ctr_at(key, nonce, data, offset=0):
    """AES-CTR (IV = nonce || 0^8) over `data` that starts at byte `offset`."""
    counter = (int.from_bytes(nonce + bytes(8), "big") + offset // 16) % (1 << 128)
    c = Cipher(algorithms.AES(key), modes.CTR(counter.to_bytes(16, "big"))).encryptor()
    skip = offset % 16
    return (c.update(bytes(skip) + data) + c.finalize())[skip:]


def pad16(data):
    return data + bytes((-len(data)) % 16)


def pattern(length, seed=0):
    return bytes((i * 31 + 7 + seed) & 0xFF for i in range(length))


def fold(raw32):
    return xor(raw32[:16], raw32[16:])


def meta_mac(aes_key, nonce, data):
    ecb = Cipher(algorithms.AES(aes_key), modes.ECB()).encryptor()
    chunk, off, acc = 131072, 0, bytes(16)
    while off < len(data):
        part = data[off:off + chunk]
        cbc = Cipher(algorithms.AES(aes_key), modes.CBC(nonce + nonce)).encryptor()
        cur = cbc.update(pad16(part))[-16:]
        acc = ecb.update(xor(acc, cur))
        off += len(part)
        chunk = min(chunk + 131072, 1048576)
    return bytes([acc[0] ^ acc[4], acc[1] ^ acc[5], acc[2] ^ acc[6], acc[3] ^ acc[7],
                  acc[8] ^ acc[12], acc[9] ^ acc[13], acc[10] ^ acc[14], acc[11] ^ acc[15]])


def attr_plain(obj):
    raw = json.dumps(obj, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return raw, pad16(b"MEGA" + raw.encode())


def attr_enc(key, obj):
    return b64(cbc_enc_zero_iv(key, attr_plain(obj)[1]))


def attr_dec(key, text):
    dec = cbc_dec_zero_iv(key, unb64(text))
    assert dec.startswith(b"MEGA")
    return dec[4:].rstrip(b"\0").decode()


def mpi(value: int) -> bytes:
    length = (value.bit_length() + 7) // 8
    return value.bit_length().to_bytes(2, "big") + value.to_bytes(length, "big")


def read_mpi(blob):
    bits = int.from_bytes(blob[:2], "big")
    size = (bits + 7) // 8
    return int.from_bytes(blob[2:2 + size], "big"), blob[2 + size:]


def rust_bytes(data):
    return "[" + ", ".join(f"0x{b:02x}" for b in data) + "]"


def const_bytes(name, data):
    return f"pub const {name}: [u8; {len(data)}] = {rust_bytes(data)};"


def const_str(name, text):
    return f"pub const {name}: &str = {json.dumps(text, ensure_ascii=False)};"


def key16(seed):
    return bytes((seed * 17 + i * 7 + 3) & 0xFF for i in range(16))


def key32(seed):
    return bytes((seed * 17 + i * 7 + 3) & 0xFF for i in range(32))


def main():
    out = ["// @generated by tests/fixtures/mega/generate_manage_vectors.py -- do not edit.",
           "// Independent (Python hashlib + `cryptography`) vectors for MEGA node management.", ""]

    master = bytes((i * 7 + 3) & 0xFF for i in range(16))
    user = "MNGUSER1"
    out.append(const_bytes("MASTER_KEY", master))
    out.append(const_str("USER_HANDLE", user))

    # --- upload encryption ---------------------------------------------------
    aes_key = bytes((i * 13 + 5) & 0xFF for i in range(16))
    nonce = bytes((i * 19 + 11) & 0xFF for i in range(8))
    out.append(const_bytes("UPLOAD_AES_KEY", aes_key))
    out.append(const_bytes("UPLOAD_NONCE", nonce))
    out.append("pub const UPLOAD_CASES: &[(usize, &str, [u8; 8], [u8; 32], &str)] = &[")
    upload_checks = []
    cipher_3m = b""
    for length in (0, 100_000, 3_000_000):
        plain = pattern(length, 0)
        ct = ctr_at(aes_key, nonce, plain)
        mac = meta_mac(aes_key, nonce, plain)
        packed = xor(aes_key, nonce + mac) + nonce + mac
        wrapped = b64(ecb_enc(master, packed))
        out.append(f"    ({length}, {json.dumps(hashlib.sha256(ct).hexdigest())}, "
                   f"{rust_bytes(mac)}, {rust_bytes(packed)}, {json.dumps(wrapped)}),")
        upload_checks.append((plain, ct, mac, packed, wrapped))
        if length == 3_000_000:
            cipher_3m = ct
    out.append("];")
    offset, seg = 393216, 100_000
    segment = cipher_3m[offset:offset + seg]
    out.append(const_str("UPLOAD_OFFSET_CT_SHA256", hashlib.sha256(segment).hexdigest()))

    # --- attribute encryption ------------------------------------------------
    folder_key = key16(11)
    file_key = key32(12)
    out.append(const_bytes("ATTR_FOLDER_KEY", folder_key))
    out.append(const_bytes("ATTR_FILE_KEY", file_key))
    attr_objs = [
        ({"n": "plain.txt"}, "folder"),
        ({"n": "文档 ✓.txt"}, "file"),
        ({"n": 'a"b\\c'}, "folder"),
        ({"c": "FINGERPRINT", "n": "keep.bin", "rr": "RESTORE1"}, "file"),
    ]
    out.append("pub const ATTR_CASES: &[(&str, &str, &str)] = &[")
    attr_rows = []
    for obj, source in attr_objs:
        raw, _ = attr_plain(obj)
        key = folder_key if source == "folder" else fold(file_key)
        enc = attr_enc(key, obj)
        attr_rows.append((raw, source, enc))
        out.append(f"    (r####\"{raw}\"####, {json.dumps(source)}, {json.dumps(enc)}),")
    out.append("];")
    node_key = key16(13)
    node_wrapped = b64(ecb_enc(master, node_key))
    out.append(const_bytes("FOLDER_NODE_KEY", node_key))
    out.append(const_str("FOLDER_NODE_KEY_WRAPPED_B64", node_wrapped))

    # --- folder export (outgoing share) ---------------------------------------
    share_handle = "EXPSHARE"
    share_key = key16(21)
    out.append(const_str("SHARE_HANDLE", share_handle))
    out.append(const_bytes("SHARE_KEY", share_key))
    share_ok = b64(ecb_enc(master, share_key))
    share_ha = b64(ecb_enc(master, (share_handle * 2).encode()))
    out.append(const_str("SHARE_OK_B64", share_ok))
    out.append(const_str("SHARE_HA_B64", share_ha))
    cr_nodes = [(share_handle, key16(22)), ("SHFILE01", key32(23)), ("SHFILE02", key32(24))]
    out.append("pub const SHARE_CR_NODES: &[(&str, &[u8])] = &[")
    for handle, key in cr_nodes:
        out.append(f"    ({json.dumps(handle)}, &{rust_bytes(key)}),")
    out.append("];")
    triples = []
    for index, (_, key) in enumerate(cr_nodes):
        triples += [0, index, b64(ecb_enc(share_key, key))]
    cr = [[share_handle], [h for h, _ in cr_nodes], triples]
    cr_json = json.dumps(cr, ensure_ascii=False, separators=(",", ":"))
    out.append(f"pub const SHARE_CR_JSON: &str = r####\"{cr_json}\"####;")

    # --- incoming share ---------------------------------------------------------
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    numbers = key.private_numbers()
    p, q, d = numbers.p, numbers.q, numbers.d
    n, e = p * q, numbers.public_numbers.e
    u = pow(q, -1, p)
    privk = b64(ecb_enc(master, pad16(mpi(p) + mpi(q) + mpi(d) + mpi(u))))
    out.append(const_str("PRIVK_B64", privk))

    incoming_key = bytes([0x2b]) + bytes((i * 29 + 17) & 0xFF for i in range(15))
    while True:
        m_bytes = incoming_key + secrets.token_bytes(256 - 16)
        m = int.from_bytes(m_bytes, "big")
        if m < n:
            break
    sk = b64(mpi(pow(m, e, n)))
    root_handle = "INCROOT1"
    out.append(const_bytes("INCOMING_SHARE_KEY", incoming_key))
    out.append(const_str("INCOMING_ROOT_HANDLE", root_handle))
    out.append(const_str("INCOMING_SK_B64", sk))

    incoming_folder_key = key16(31)
    incoming_file_key = key32(32)
    incoming_file_handle = "INCFILE1"
    own_folder_key = key16(33)
    drive, inbox, rubbish, own_handle = "DRIVROOT", "INBOXHDL", "RUBBISH1", "OWNFOLD1"
    root_name, file_name, own_name = "Shared Docs", "inside.txt", "Mine"
    owner = "OWNERHDL"
    nodes = [
        {"h": drive, "p": "", "u": user, "t": 2, "ts": 1600000000},
        {"h": inbox, "p": "", "u": user, "t": 3, "ts": 1600000000},
        {"h": rubbish, "p": "", "u": user, "t": 4, "ts": 1600000000},
        {"h": root_handle, "p": "PARENTOFF", "u": owner, "su": owner, "t": 1, "r": 0,
         "sk": sk,
         "k": f"{root_handle}:" + b64(ecb_enc(incoming_key, incoming_folder_key)),
         "a": attr_enc(incoming_folder_key, {"n": root_name}), "ts": 1600000100},
        {"h": incoming_file_handle, "p": root_handle, "u": owner, "t": 0, "s": 1234,
         "k": f"{root_handle}:" + b64(ecb_enc(incoming_key, incoming_file_key)),
         "a": attr_enc(fold(incoming_file_key), {"n": file_name}), "ts": 1600000200},
        {"h": own_handle, "p": drive, "u": user, "t": 1,
         "k": f"{user}:" + b64(ecb_enc(master, own_folder_key)),
         "a": attr_enc(own_folder_key, {"n": own_name}), "ts": 1600000300},
    ]
    listing = {"f": nodes}
    out.append(const_bytes("INCOMING_FOLDER_NODE_KEY", incoming_folder_key))
    out.append(const_bytes("INCOMING_FILE_NODE_KEY", incoming_file_key))
    out.append(const_str("INCOMING_FILE_HANDLE", incoming_file_handle))
    out.append(const_str("INCOMING_ROOT_NAME", root_name))
    out.append(const_str("INCOMING_FILE_NAME", file_name))
    out.append(const_str("OWN_FOLDER_HANDLE", own_handle))
    out.append(const_str("OWN_FOLDER_NAME", own_name))
    out.append(const_str("DRIVE_ROOT_HANDLE", drive))
    out.append(const_str("RUBBISH_HANDLE", rubbish))
    out.append(const_str("INBOX_HANDLE", inbox))
    out.append("pub const INCOMING_LISTING: &str = r####\"" + json.dumps(listing, ensure_ascii=False) + "\"####;")

    # --- self check: re-derive everything the other way round ------------------
    for plain, ct, mac, packed, wrapped in upload_checks:
        assert ctr_at(aes_key, nonce, ct) == plain
        assert meta_mac(aes_key, nonce, plain) == mac
        assert packed[16:24] == nonce and packed[24:] == mac
        assert xor(packed[:16], packed[16:]) == aes_key
        assert ecb_dec(master, unb64(wrapped)) == packed
    plain3 = pattern(3_000_000, 0)
    assert ctr_at(aes_key, nonce, plain3[offset:offset + seg], offset) == segment
    assert ctr_at(aes_key, nonce, plain3[1001:5003], 1001) == cipher_3m[1001:5003]
    for raw, source, enc in attr_rows:
        k = folder_key if source == "folder" else fold(file_key)
        assert attr_dec(k, enc) == raw
        json.loads(raw)
    assert ecb_dec(master, unb64(node_wrapped)) == node_key
    assert ecb_dec(master, unb64(share_ok)) == share_key
    assert ecb_dec(master, unb64(share_ha)) == (share_handle * 2).encode()
    assert cr[0] == [share_handle] and cr[1] == [h for h, _ in cr_nodes]
    for i, (h, k) in enumerate(cr_nodes):
        assert len(h) == 8
        assert cr[2][3 * i:3 * i + 2] == [0, i]
        assert ecb_dec(share_key, unb64(cr[2][3 * i + 2])) == k
    assert [len(k) for _, k in cr_nodes] == [16, 32, 32]
    blob = ecb_dec(master, unb64(privk))
    pp, rest = read_mpi(blob)
    qq, rest = read_mpi(rest)
    dd, rest = read_mpi(rest)
    uu, rest = read_mpi(rest)
    assert (pp, qq, dd) == (p, q, d) and uu == pow(q, -1, p) and not rest.strip(b"\0")
    c, _ = read_mpi(unb64(sk))
    assert pow(c, d, n).to_bytes(256, "big")[:16] == incoming_key
    by_handle = {x["h"]: x for x in nodes}
    assert all(len(h) == 8 for h in by_handle)
    root = by_handle[root_handle]
    assert root["p"] not in by_handle and root["sk"] == sk
    head, wrapped = root["k"].split(":")
    assert head == root_handle and ecb_dec(incoming_key, unb64(wrapped)) == incoming_folder_key
    assert json.loads(attr_dec(incoming_folder_key, root["a"])) == {"n": root_name}
    child = by_handle[incoming_file_handle]
    head, wrapped = child["k"].split(":")
    assert head == root_handle and ecb_dec(incoming_key, unb64(wrapped)) == incoming_file_key
    assert json.loads(attr_dec(fold(incoming_file_key), child["a"])) == {"n": file_name}
    own = by_handle[own_handle]
    head, wrapped = own["k"].split(":")
    assert head == user and ecb_dec(master, unb64(wrapped)) == own_folder_key
    assert json.loads(attr_dec(own_folder_key, own["a"])) == {"n": own_name}

    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
