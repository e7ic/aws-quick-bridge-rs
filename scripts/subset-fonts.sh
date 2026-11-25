#!/bin/sh
set -eu

# 按界面与常用简体汉字子集化 MiSans 字体，仅保留实际使用的 Regular / Medium 两套字重，
# 并将 OS/2 usWeightClass 规范化为标准 CSS 字重（400 / 500），保证 macOS 与 Windows 一致匹配。
# 用法：sh scripts/subset-fonts.sh [原版 MiSans ttf 目录]

cd "$(dirname "$0")/.."
src_dir="${1:-/Users/e7ic/Downloads/MiSans/ttf}"
out_dir="assets/fonts"

if ! command -v hb-subset >/dev/null 2>&1; then
    printf '%s\n' '需要安装 hb-subset（macOS 可执行：brew install harfbuzz）。' >&2
    exit 1
fi

for f in MiSans-Regular.ttf MiSans-Medium.ttf; do
    if [ ! -f "$src_dir/$f" ]; then
        printf '找不到源字体：%s/%s\n' "$src_dir" "$f" >&2
        exit 1
    fi
done

mkdir -p "$out_dir"

python3 - "$src_dir" "$out_dir" <<'PY'
import glob
import gzip
import os
import struct
import subprocess
import sys
import tempfile

src_dir, out_dir = sys.argv[1], sys.argv[2]

# 收集 ASCII、全角与中文标点、常用排版符号、源码内全部字符及 GB2312 一级常用汉字（3755 字）
chars = set(chr(c) for c in range(0x20, 0x7F))
chars.update(chr(c) for c in range(0x3000, 0x3040))
chars.update(chr(c) for c in range(0xFF01, 0xFF5F))
chars.update("—–…·•“”‘’°±×÷→←↑↓›‹✓✕●○◆◇■□▲△▼▽★☆①②③④⑤⑥⑦⑧⑨⑩")

for path in glob.glob("src/**/*.rs", recursive=True):
    with open(path, encoding="utf-8") as fp:
        chars.update(fp.read())

for qu in range(16, 56):
    for wei in range(1, 95):
        try:
            chars.add(bytes([qu + 0xA0, wei + 0xA0]).decode("gb2312"))
        except UnicodeDecodeError:
            pass

# 过滤控制字符
chars = {ch for ch in chars if ord(ch) >= 0x20}


def ttf_checksum(buf: bytes) -> int:
    pad = (-len(buf)) % 4
    padded = buf + (b"\x00" * pad)
    return sum(struct.unpack(f">{len(padded) // 4}I", padded)) & 0xFFFFFFFF


def normalize_os2_weight(path: str, weight: int) -> None:
    data = bytearray(open(path, "rb").read())
    _, num_tables = struct.unpack(">IH", data[:6])
    head_info = None
    for idx in range(num_tables):
        rec = 12 + idx * 16
        tag, _, off, length = struct.unpack(">4sIII", data[rec : rec + 16])
        if tag == b"OS/2":
            struct.pack_into(">H", data, off + 4, weight)
            struct.pack_into(">I", data, rec + 4, ttf_checksum(bytes(data[off : off + length])))
        elif tag == b"head":
            head_info = (rec, off, length)
    if head_info is None:
        raise RuntimeError(f"missing head table in {path}")
    head_rec, head_off, head_len = head_info
    struct.pack_into(">I", data, head_off + 8, 0)
    struct.pack_into(
        ">I", data, head_rec + 4, ttf_checksum(bytes(data[head_off : head_off + head_len]))
    )
    adj = (0xB1B0AFBA - ttf_checksum(bytes(data))) & 0xFFFFFFFF
    struct.pack_into(">I", data, head_off + 8, adj)
    with open(path, "wb") as fp:
        fp.write(data)


with tempfile.TemporaryDirectory() as tmpdir:
    text_file = os.path.join(tmpdir, "chars.txt")
    with open(text_file, "w", encoding="utf-8") as fp:
        fp.write("".join(sorted(chars)))

    total_raw = 0
    total_gz = 0
    for filename, target_weight in [("MiSans-Regular.ttf", 400), ("MiSans-Medium.ttf", 500)]:
        src_path = os.path.join(src_dir, filename)
        dst_path = os.path.join(out_dir, filename)
        subprocess.run(
            [
                "hb-subset",
                src_path,
                f"--text-file={text_file}",
                "--layout-features=*",
                "--name-IDs=*",
                "--name-legacy",
                f"--output-file={dst_path}",
            ],
            check=True,
        )
        normalize_os2_weight(dst_path, target_weight)
        raw = open(dst_path, "rb").read()
        gz = gzip.compress(raw, compresslevel=9)
        total_raw += len(raw)
        total_gz += len(gz)
        print(
            f"{filename}: weight={target_weight}, raw={len(raw)/1024:.1f} KB, gz={len(gz)/1024:.1f} KB"
        )
    print(
        f"总计（{len(chars)} 字符）：raw={total_raw/1024/1024:.2f} MB, gz={total_gz/1024/1024:.2f} MB"
    )
PY
