#!/bin/sh
# 安装 CAYW 发行包中的 cayw、cayw-client 和 cayw-tui（发行包内文件名为 install.sh）。
# 只支持 macOS arm64；不联网、不使用 sudo、不修改 shell 配置。

set -eu

PREFIX=
PREFIX_SET=0
FORCE=0
TMP_RUNTIME=
TMP_CLIENT=
TMP_TUI=
BACKUP_RUNTIME=
BACKUP_CLIENT=
BACKUP_TUI=

die() {
    printf '安装失败：%s\n' "$*" >&2
    exit 1
}

cleanup() {
    for file in "$TMP_RUNTIME" "$TMP_CLIENT" "$TMP_TUI" "$BACKUP_RUNTIME" "$BACKUP_CLIENT" "$BACKUP_TUI"; do
        [ -z "$file" ] || rm -f "$file" 2>/dev/null || :
    done
}
trap cleanup 0

usage() {
    cat <<EOF
用法：${0##*/} [--prefix DIR] [--force]

把发行包中的 cayw、cayw-client 和 cayw-tui 安装到 PREFIX/bin。
默认 PREFIX 为 \$HOME/.local，即 \$HOME/.local/bin。

选项：
  --prefix DIR  安装到 DIR/bin
  --force       目标位置已有不同程序时替换；不加该选项不会改动它
  --help, -h    显示本说明

只支持 macOS arm64（Apple Silicon）。脚本不联网、不使用 sudo，
不修改 shell 配置，也不读写 ~/.cayw 或任何模型凭据。
安装前检查三个程序可执行并核对 SHA256SUMS；失败时不改动已有程序。
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --help|-h)
            usage
            exit 0
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --prefix)
            [ $# -ge 2 ] || die "--prefix 需要一个目录参数"
            PREFIX=$2
            PREFIX_SET=1
            shift 2
            ;;
        --prefix=*)
            PREFIX=${1#--prefix=}
            PREFIX_SET=1
            shift
            ;;
        *)
            die "未知参数：$1；使用 --help 查看用法"
            ;;
    esac
done

if [ "$PREFIX_SET" -eq 1 ]; then
    [ -n "$PREFIX" ] || die "--prefix 需要一个非空目录"
else
    [ -n "${HOME:-}" ] || die "HOME 未设置；请用 --prefix DIR 指定安装目录"
    PREFIX=$HOME/.local
fi
case $PREFIX in
    /*) ;;
    *) PREFIX=$(pwd)/$PREFIX ;;
esac
BIN=$PREFIX/bin
DEST_RUNTIME=$BIN/cayw
DEST_CLIENT=$BIN/cayw-client
DEST_TUI=$BIN/cayw-tui

SYSTEM=$(uname -s)
MACHINE=$(uname -m)
[ "$SYSTEM" = Darwin ] || die "只支持 macOS（Darwin），当前系统是 $SYSTEM"
[ "$MACHINE" = arm64 ] || die "只支持 Apple Silicon（arm64），当前架构是 $MACHINE"

command -v shasum >/dev/null 2>&1 || die "找不到 shasum，无法校验发行包"
SCRIPT_DIR=$(CDPATH= cd -P "$(dirname "$0")" && pwd) || die "无法定位脚本所在目录"
RUNTIME=$SCRIPT_DIR/bin/cayw
CLIENT=$SCRIPT_DIR/bin/cayw-client
TUI=$SCRIPT_DIR/bin/cayw-tui
SUMS=$SCRIPT_DIR/SHA256SUMS

[ -f "$SUMS" ] || die "发行包不完整：找不到 $SUMS；请在解压后的发行目录中运行 ${0##*/}"
[ -f "$RUNTIME" ] || die "发行包不完整：找不到 $RUNTIME"
[ -x "$RUNTIME" ] || die "$RUNTIME 不可执行；请重新解压发行包并保留文件权限"
[ -f "$CLIENT" ] || die "发行包不完整：找不到 $CLIENT"
[ -x "$CLIENT" ] || die "$CLIENT 不可执行；请重新解压发行包并保留文件权限"
[ -f "$TUI" ] || die "发行包不完整：找不到 $TUI"
[ -x "$TUI" ] || die "$TUI 不可执行；请重新解压发行包并保留文件权限"

# 从 SHA256SUMS 中读取指定文件的预期摘要。
expected_hash() {
    awk -v want="$1" '
        {
            name = $0
            sub(/^[0-9A-Fa-f]+[[:space:]]+\*?/, "", name)
            sub(/^\.\//, "", name)
            if (name == want) {
                print tolower($1)
                found = 1
                exit
            }
        }
        END { if (!found) exit 1 }
    ' "$SUMS"
}

sha_of() {
    shasum -a 256 "$1" | awk '{ print tolower($1) }'
}

NEW_RUNTIME=$(expected_hash bin/cayw) || die "SHA256SUMS 中没有 bin/cayw 的记录；发行包不完整"
NEW_CLIENT=$(expected_hash bin/cayw-client) || die "SHA256SUMS 中没有 bin/cayw-client 的记录；发行包不完整"
NEW_TUI=$(expected_hash bin/cayw-tui) || die "SHA256SUMS 中没有 bin/cayw-tui 的记录；发行包不完整"
[ "$(sha_of "$RUNTIME")" = "$NEW_RUNTIME" ] || die "SHA256 校验失败：bin/cayw 与 SHA256SUMS 不一致；发行包可能损坏。未改动任何已安装程序"
[ "$(sha_of "$CLIENT")" = "$NEW_CLIENT" ] || die "SHA256 校验失败：bin/cayw-client 与 SHA256SUMS 不一致；发行包可能损坏。未改动任何已安装程序"
[ "$(sha_of "$TUI")" = "$NEW_TUI" ] || die "SHA256 校验失败：bin/cayw-tui 与 SHA256SUMS 不一致；发行包可能损坏。未改动任何已安装程序"
printf '校验通过：bin/cayw、bin/cayw-client、bin/cayw-tui 与 SHA256SUMS 一致。\n'

check_target() {
    dest=$1
    new_hash=$2
    if [ -e "$dest" ] || [ -L "$dest" ]; then
        [ -f "$dest" ] || die "$dest 不是普通文件，不能覆盖；未改动任何程序"
        old_hash=$(sha_of "$dest") || die "无法读取已有程序 $dest；未改动任何程序"
        if [ "$old_hash" != "$new_hash" ] && [ "$FORCE" -ne 1 ]; then
            die "$dest 已存在且与发行包不同；确认替换请加 --force（原程序未改动）"
        fi
    fi
}
check_target "$DEST_RUNTIME" "$NEW_RUNTIME"
check_target "$DEST_CLIENT" "$NEW_CLIENT"
check_target "$DEST_TUI" "$NEW_TUI"

mkdir -p "$BIN" || die "无法创建 $BIN"
[ -d "$BIN" ] || die "$BIN 不是目录"
[ -w "$BIN" ] || die "没有写入 $BIN 的权限（脚本不使用 sudo）"

if [ -e "$DEST_RUNTIME" ] || [ -L "$DEST_RUNTIME" ]; then
    BACKUP_RUNTIME=$BIN/.cayw-install.$$.cayw.bak
    cp -p "$DEST_RUNTIME" "$BACKUP_RUNTIME" || die "无法备份 $DEST_RUNTIME；未改动任何程序"
fi
if [ -e "$DEST_CLIENT" ] || [ -L "$DEST_CLIENT" ]; then
    BACKUP_CLIENT=$BIN/.cayw-install.$$.cayw-client.bak
    cp -p "$DEST_CLIENT" "$BACKUP_CLIENT" || die "无法备份 $DEST_CLIENT；未改动任何程序"
fi
if [ -e "$DEST_TUI" ] || [ -L "$DEST_TUI" ]; then
    BACKUP_TUI=$BIN/.cayw-install.$$.cayw-tui.bak
    cp -p "$DEST_TUI" "$BACKUP_TUI" || die "无法备份 $DEST_TUI；未改动任何程序"
fi

TMP_RUNTIME=$BIN/.cayw-install.$$.cayw.tmp
TMP_CLIENT=$BIN/.cayw-install.$$.cayw-client.tmp
TMP_TUI=$BIN/.cayw-install.$$.cayw-tui.tmp
cp "$RUNTIME" "$TMP_RUNTIME" || die "无法复制 cayw 到 $BIN"
cp "$CLIENT" "$TMP_CLIENT" || die "无法复制 cayw-client 到 $BIN"
cp "$TUI" "$TMP_TUI" || die "无法复制 cayw-tui 到 $BIN"
chmod 755 "$TMP_RUNTIME" "$TMP_CLIENT" "$TMP_TUI" || die "无法设置可执行权限；未改动任何程序"
[ "$(sha_of "$TMP_RUNTIME")" = "$NEW_RUNTIME" ] || die "复制后校验失败：cayw；未改动任何程序"
[ "$(sha_of "$TMP_CLIENT")" = "$NEW_CLIENT" ] || die "复制后校验失败：cayw-client；未改动任何程序"
[ "$(sha_of "$TMP_TUI")" = "$NEW_TUI" ] || die "复制后校验失败：cayw-tui；未改动任何程序"
"$TMP_RUNTIME" --help >/dev/null 2>&1 || die "自检失败：cayw --help 未正常退出；未改动任何程序"
"$TMP_CLIENT" --help >/dev/null 2>&1 || die "自检失败：cayw-client --help 未正常退出；未改动任何程序"
"$TMP_TUI" --help >/dev/null 2>&1 || die "自检失败：cayw-tui --help 未正常退出；未改动任何程序"

rollback() {
    if [ -n "$BACKUP_RUNTIME" ]; then
        if mv -f "$BACKUP_RUNTIME" "$DEST_RUNTIME"; then
            BACKUP_RUNTIME=
        else
            printf '警告：无法恢复原程序；备份保留在 %s\n' "$BACKUP_RUNTIME" >&2
            BACKUP_RUNTIME=
        fi
    elif [ -f "$DEST_RUNTIME" ]; then
        rm -f "$DEST_RUNTIME"
    fi
    if [ -n "$BACKUP_CLIENT" ]; then
        if mv -f "$BACKUP_CLIENT" "$DEST_CLIENT"; then
            BACKUP_CLIENT=
        else
            printf '警告：无法恢复原程序；备份保留在 %s\n' "$BACKUP_CLIENT" >&2
            BACKUP_CLIENT=
        fi
    elif [ -f "$DEST_CLIENT" ]; then
        rm -f "$DEST_CLIENT"
    fi
    if [ -n "$BACKUP_TUI" ]; then
        if mv -f "$BACKUP_TUI" "$DEST_TUI"; then
            BACKUP_TUI=
        else
            printf '警告：无法恢复原程序；备份保留在 %s\n' "$BACKUP_TUI" >&2
            BACKUP_TUI=
        fi
    elif [ -f "$DEST_TUI" ]; then
        rm -f "$DEST_TUI"
    fi
}

mv -f "$TMP_RUNTIME" "$DEST_RUNTIME" || { rollback; die "无法写入 $DEST_RUNTIME"; }
mv -f "$TMP_CLIENT" "$DEST_CLIENT" || { rollback; die "无法写入 $DEST_CLIENT"; }
mv -f "$TMP_TUI" "$DEST_TUI" || { rollback; die "无法写入 $DEST_TUI"; }

if "$DEST_RUNTIME" --help >/dev/null 2>&1 && "$DEST_CLIENT" --help >/dev/null 2>&1 && "$DEST_TUI" --help >/dev/null 2>&1; then
    :
else
    rollback
    die "安装后自检失败：cayw、cayw-client 或 cayw-tui 的 --help 未正常退出；已尝试恢复原程序"
fi

[ -z "$BACKUP_RUNTIME" ] || rm -f "$BACKUP_RUNTIME"
[ -z "$BACKUP_CLIENT" ] || rm -f "$BACKUP_CLIENT"
[ -z "$BACKUP_TUI" ] || rm -f "$BACKUP_TUI"
BACKUP_RUNTIME=
BACKUP_CLIENT=
BACKUP_TUI=

printf '自检通过：cayw、cayw-client 和 cayw-tui 的 --help 正常。\n'
printf '已安装 cayw：%s\n' "$DEST_RUNTIME"
printf '已安装 cayw-client：%s\n' "$DEST_CLIENT"
printf '已安装 cayw-tui：%s\n' "$DEST_TUI"

case ":${PATH:-}:" in
    *":$BIN:"*)
        printf '\n下一步：\n'
        ;;
    *)
        printf '\n提示：%s 不在 PATH 中。请执行下面一行，或加入你的 shell 配置后重新打开终端：\n' "$BIN"
        printf '  export PATH="%s:$PATH"\n' "$BIN"
        printf '\nPATH 生效后运行：\n'
        ;;
esac
printf '  cayw configure   # 配置模型服务；只保存凭据的环境变量名\n'
printf '  cayw serve       # 在当前目录启动本机服务\n'
printf '  cayw tui         # 另一终端进入同一工作区，打开交互式终端界面\n'
printf '\n没有模型配置时，cayw serve 在终端会询问是否运行 cayw configure；非交互环境会打印配置路径和 cayw configure 提示。\n'
