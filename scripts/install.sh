#!/bin/sh
# ilp 安装脚本。
#
# 设计取舍：
#
# 1. **校验和 + 签名都要验。** 只验 sha256 没有意义——校验和文件本身是从
#    同一个地方下载的，中间人换了二进制也能顺手换掉校验和。签名才是把
#    信任锚定到发布方公钥的那一步。
# 2. **不猜平台。** 认不出就列可选值然后退出，而不是挑一个"大概能跑"的。
#    装错的二进制报的错（GLIBC not found / exec format error）比"不支持"难懂得多。
# 3. **默认装到 /usr/local/bin。** 需要 sudo 时明确说要 sudo，而不是
#    静默装到某个不在 PATH 里的角落。
# 4. **不 curl | sh 执行远端逻辑。** 这个脚本只做下载、校验、安装，
#    可审计；下载的东西先落盘再执行。

set -eu

REPO="${ILP_REPO:-ILP-Project/ILP}"
VERSION="${ILP_VERSION:-latest}"
PREFIX="${ILP_PREFIX:-/usr/local/bin}"
# 与 config.json 的 plugin_trusted_keys 同一套密钥。装二进制和装插件
# 用两个信任根只会让用户多配一次、多错一次。
PUBKEY="${ILP_PUBKEY:-}"

die() {
	echo "错误: $*" >&2
	exit 1
}

info() {
	echo "==> $*"
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "需要 $1，但没找到"
}

need uname
need tar
need curl

os=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$os" in
linux) os=linux ;;
darwin) os=macos ;;
*) die "不支持的系统: $os（目前只发 Linux 与 macOS）" ;;
esac

arch=$(uname -m)
case "$arch" in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) die "不支持的架构: $arch" ;;
esac

# ILP_PLATFORM 覆盖，仅供自测时在任意机器上验证非本机平台的流程。
platform="${ILP_PLATFORM:-$os-$arch}"

# 校验放在覆盖之后：先算 $os-$arch 再允许覆盖，覆盖值就能绕过白名单，
# 拿一个不存在的产物名去下载，最后报"下载失败"而不是"没有这个平台"。
case "$platform" in
linux-x86_64 | linux-aarch64 | macos-x86_64 | macos-aarch64) ;;
*) die "没有 $platform 的产物。可选: linux-x86_64, linux-aarch64, macos-x86_64, macos-aarch64" ;;
esac

# ILP_BASE_URL 覆盖下载根地址，供离线/镜像/自测使用。
if [ -n "${ILP_BASE_URL:-}" ]; then
	base="$ILP_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
	base="https://github.com/$REPO/releases/latest/download"
else
	# 允许 v0.1.0 和 0.1.0 两种写法，用户不该为这点小事困惑。
	case "$VERSION" in
	v*) tag="$VERSION" ;;
	*) tag="v$VERSION" ;;
	esac
	base="https://github.com/$REPO/releases/download/$tag"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

info "下载 $platform ($VERSION)"
curl -fsSL -o "$tmp/ilp.tar.gz" "$base/ilp-$VERSION-$platform.tar.gz" ||
	die "下载失败: $base/ilp-$VERSION-$platform.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "下载校验和失败"
curl -fsSL -o "$tmp/SHA256SUMS.sig" "$base/SHA256SUMS.sig" || die "下载签名失败"

# 先验签名，再验校验和。顺序不能反：校验和来自同一来源，未经验签的
# 校验和只能防传输损坏，防不了篡改。
if [ -n "$PUBKEY" ]; then
	need python3
	info "验证 SHA256SUMS 签名"
	ILP_PUBKEY="$PUBKEY" python3 - "$tmp" <<'PY'
import base64
import json
import os
import sys

tmp = sys.argv[1]
raw = open(os.path.join(tmp, "SHA256SUMS"), "rb").read()
try:
    sigs = json.load(open(os.path.join(tmp, "SHA256SUMS.sig")))
except Exception as exc:  # noqa: BLE001
    sys.exit(f"读取签名失败: {exc}")
if isinstance(sigs, dict):
    sigs = [sigs]

try:
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
except ImportError:
    # 没有 cryptography 就没法验签。这不能降级成"跳过验签继续装"——
    # 那等于用户以为验过了，实际没有。也不能硬失败：很多机器上
    # pip install 是被策略禁掉的。给出两条明确的路。
    sys.exit(
        "缺少 python3 的 cryptography 包，无法验证签名。\n"
        "    装它：pip install cryptography\n"
        "    或不验签安装（不推荐）：不设 ILP_PUBKEY 重跑"
    )

# 公钥格式与 config.json 里的 plugin_trusted_keys 一致：key_id:base64
wanted = []
for item in os.environ["ILP_PUBKEY"].split(","):
    item = item.strip()
    if not item:
        continue
    key_id, _, b64 = item.partition(":")
    wanted.append((key_id.strip(), base64.b64decode(b64)))

if not wanted:
    sys.exit("ILP_PUBKEY 为空")

ok = []
for entry in sigs:
    for key_id, pub in wanted:
        if entry.get("key_id") != key_id:
            continue
        try:
            Ed25519PublicKey.from_public_bytes(pub).verify(
                base64.b64decode(entry["signature"]), raw
            )
            ok.append(key_id)
        except Exception:  # noqa: BLE001
            pass

if not ok:
    sys.exit("签名验证失败：SHA256SUMS 不是由可信密钥签发的")
print(f"签名有效（{', '.join(ok)}）")
PY
else
	echo "==> 未配置 ILP_PUBKEY，跳过签名验证" >&2
	echo "    设 ILP_PUBKEY='key_id:base64' 以校验发布方签名" >&2
fi

info "校验文件完整性"
expected=$(grep " ilp-$VERSION-$platform.tar.gz$" "$tmp/SHA256SUMS" | awk '{print $1}')
[ -n "$expected" ] || die "SHA256SUMS 里没有 ilp-$VERSION-$platform.tar.gz"
actual=$(cd "$tmp" && { sha256sum ilp.tar.gz 2>/dev/null || shasum -a 256 ilp.tar.gz; } | awk '{print $1}')
[ "$expected" = "$actual" ] || die "校验和不匹配: 期望 $expected，实得 $actual"

info "解包"
tar -xzf "$tmp/ilp.tar.gz" -C "$tmp"
[ -f "$tmp/ilp" ] || die "包里没有 ilp 可执行文件"

info "安装到 $PREFIX/ilp"
# 目录不存在时先建，而不是一头撞上 sudo —— "目录没建" 和 "没权限" 是
# 两回事，用 sudo 去解前者只会把问题掩盖成权限问题。
if [ ! -d "$PREFIX" ]; then
	if mkdir -p "$PREFIX" 2>/dev/null; then
		:
	else
		echo "    需要 sudo 创建 $PREFIX" >&2
		sudo mkdir -p "$PREFIX"
	fi
fi

if [ -w "$PREFIX" ]; then
	install -m 755 "$tmp/ilp" "$PREFIX/ilp"
else
	echo "    需要 sudo 写入 $PREFIX" >&2
	sudo install -m 755 "$tmp/ilp" "$PREFIX/ilp"
fi

info "完成: $("$PREFIX/ilp" --version)"
echo
echo "下一步:"
echo "  ilp doctor                    # 检查环境"
echo "  ilp plugin install fanqie    # 装一个插件"
