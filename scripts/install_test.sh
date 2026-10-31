#!/bin/sh
# install.sh 的自测。不需要网络，也不碰真实 Releases。
#
# 为什么值得单独写：安装脚本是唯一一个"用户会直接执行"的东西，而它做的事
# 恰恰是最危险的那类——下载别人的字节然后装进 PATH。它的失败模式不是
# "报个错"，而是"装了个被篡改的二进制还告诉用户成功了"。所以这里验的不是
# 正常路径能跑通，而是**异常路径必须拒绝**。
#
# 需要：一个带 cryptography 的 python3。用 PATH 里的那个，或在自测时把
# venv 的 bin 放到 PATH 前面。

set -eu

here=$(cd "$(dirname "$0")" && pwd)
script="$here/install.sh"
repo=$(cd "$here/.." && pwd)

[ -f "$script" ] || { echo "找不到 install.sh" >&2; exit 1; }

work=$(mktemp -d)
server_pid=
cleanup() {
	[ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
	rm -rf "$work"
}
trap cleanup EXIT INT TERM

# 用现成二进制模拟发布产物；release 构建太慢，自测不需要真跑一遍。
src="$repo/target/release/ilp"
[ -x "$src" ] || src="$repo/target/debug/ilp"
[ -x "$src" ] || { echo "先构建一次 ilp（cargo build）" >&2; exit 1; }

mkdir -p "$work/dl" "$work/stage"
cp "$src" "$work/stage/ilp"
cp "$repo/LICENSE" "$work/stage/"
printf 'ilp 自测用 README\n' >"$work/stage/README.md"

dist="$work/dl"
name="ilp-latest-linux-x86_64.tar.gz"
tar -czf "$dist/$name" -C "$work/stage" ilp LICENSE README.md
(cd "$dist" && sha256sum "$name" >SHA256SUMS)

PUBKEY=$(
	python3 - "$dist" <<'PY'
import base64
import json
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

dist = sys.argv[1]
key = Ed25519PrivateKey.from_private_bytes(bytes(range(32)))
pub = key.public_key().public_bytes(
    encoding=serialization.Encoding.Raw,
    format=serialization.PublicFormat.Raw,
)
raw = open(f"{dist}/SHA256SUMS", "rb").read()
json.dump(
    {"key_id": "fixture", "signature": base64.b64encode(key.sign(raw)).decode()},
    open(f"{dist}/SHA256SUMS.sig", "w"),
    indent=2,
)
print("fixture:" + base64.b64encode(pub).decode())
PY
)

port=8791
python3 -m http.server "$port" --directory "$dist" >/dev/null 2>&1 &
server_pid=$!
sleep 2
base="http://127.0.0.1:$port"

fails=0
pass() { echo "  ✔ $*"; }
fail() { echo "  ✘ $*" >&2; fails=$((fails + 1)); }

# 跑一次安装，返回退出码并记录输出。
run_install() {
	ILP_BASE_URL="$base" ILP_PLATFORM=linux-x86_64 ILP_VERSION=latest \
		ILP_PREFIX="$1" ILP_PUBKEY="${2:-}" sh "$script" >"$work/out.log" 2>&1
}

echo "1. 正常安装"
if run_install "$work/bin-ok" "$PUBKEY" && [ -x "$work/bin-ok/ilp" ]; then
	pass "装上了可执行的 ilp"
else
	fail "正常路径失败"; sed 's/^/    /' "$work/out.log" >&2
fi

echo "2. 篡改二进制后校验和必须拦住"
printf 'X' >>"$work/stage/ilp"
tar -czf "$dist/$name" -C "$work/stage" ilp LICENSE README.md
if run_install "$work/bin-tamper" "$PUBKEY"; then
	fail "篡改的二进制被安装了"
elif [ -e "$work/bin-tamper/ilp" ]; then
	fail "校验失败却留下了文件"
else
	pass "拒绝安装"
fi

echo "3. 错误公钥必须被拒（签名是信任锚）"
cp "$src" "$work/stage/ilp"
tar -czf "$dist/$name" -C "$work/stage" ilp LICENSE README.md
(cd "$dist" && sha256sum "$name" >SHA256SUMS)
wrong=$(python3 -c "import base64; print('evil:'+base64.b64encode(bytes(32)).decode())")
if run_install "$work/bin-wrongkey" "$wrong"; then
	fail "用错误公钥也装成功了"
elif [ -e "$work/bin-wrongkey/ilp" ]; then
	fail "验签失败却留下了文件"
else
	pass "拒绝安装"
fi

echo "4. 不设公钥时跳过验签但仍装得上（明确降级，不是静默）"
(cd "$dist" && sha256sum "$name" >SHA256SUMS)
if run_install "$work/bin-nokey" "" && [ -x "$work/bin-nokey/ilp" ]; then
	grep -q "跳过签名验证" "$work/out.log" &&
		pass "安装成功且明确提示未验签" ||
		fail "跳过了验签但没有提示用户"
else
	fail "无公钥时安装失败"
fi

echo "5. 不存在的平台应列可选值而不是猜"
if ILP_BASE_URL="$base" ILP_PLATFORM=plan9-vax ILP_PREFIX="$work/bin-bad" \
	sh "$script" >"$work/out.log" 2>&1; then
	fail "未知平台没有报错"
else
	grep -q "可选" "$work/out.log" && pass "列出可选平台" || fail "错误信息没给出可选值"
fi

echo
if [ "$fails" -eq 0 ]; then
	echo "全部通过"
else
	echo "$fails 项失败" >&2
	exit 1
fi
