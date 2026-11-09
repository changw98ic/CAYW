#!/bin/sh
# Offline macOS arm64 installer. Never stops running CAYW processes.
set -eu

PREFIX=
PREFIX_SET=0
FORCE=0
TRANSACTION=
REPLACING=0
PROGRAMS='cayw cayw-client cayw-tui cayw-webui'

die() {
    printf '安装失败：%s\n' "$*" >&2
    exit 1
}

# Restore the whole original set, including originally absent programs. Keep all
# backups if any restore fails. EXIT covers failures as well as caught signals.
cleanup() {
    status=$?
    trap - 0
    trap '' HUP INT TERM
    if [ -n "$TRANSACTION" ]; then
        restored=1
        if [ "$REPLACING" -eq 1 ]; then
            for program in $PROGRAMS; do
                if [ -f "$TRANSACTION/old.$program" ]; then
                    # Copy, then rename: a failed rollback must retain its backup.
                    cp -p "$TRANSACTION/old.$program" "$TRANSACTION/restore.$program" &&
                        mv -f "$TRANSACTION/restore.$program" "$BIN/$program" || restored=0
                else
                    rm -f "$BIN/$program" || restored=0
                fi
            done
            if [ "$restored" -eq 1 ]; then
                printf '已恢复安装前的全部程序。\n' >&2
            else
                printf '警告：恢复未完成。备份保留在 %s；停止使用 CAYW，先按安装指南恢复。\n' "$TRANSACTION" >&2
                status=1
            fi
        fi
        if [ "$restored" -eq 1 ]; then
            rm -rf "$TRANSACTION" || status=1
        fi
    fi
    exit "$status"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

usage() {
    cat <<HELP
用法：${0##*/} [--prefix DIR] [--force]

把发行包中的 cayw、cayw-client、cayw-tui 和 cayw-webui 安装到 PREFIX/bin。
默认 PREFIX 为 \$HOME/.local，即 \$HOME/.local/bin。

选项：
  --prefix DIR  安装到 DIR/bin
  --force       替换已有的不同程序；不会绕过运行中进程或校验检查
  --help, -h    显示本说明

只支持 macOS arm64（Apple Silicon）。不联网、不使用 sudo、不修改 shell 配置，
也不读写 ~/.cayw 或任何模型凭据。请先正常退出当前用户的全部 CAYW 进程，
等待任务清理完成；安装期间不要启动 CAYW。脚本不会替你停止任务。
校验并暂存四个程序后才替换；失败或可捕获的中断会尝试恢复全部旧程序。
强制终止或断电可能留下 .cayw-install.lock 备份目录；下次安装会拒绝覆盖它。
HELP
}

while [ $# -gt 0 ]; do
    case $1 in
        --help|-h) usage; exit 0 ;;
        --force) FORCE=1; shift ;;
        --prefix)
            [ $# -ge 2 ] || die '--prefix 需要一个目录参数'
            PREFIX=$2; PREFIX_SET=1; shift 2 ;;
        --prefix=*) PREFIX=${1#--prefix=}; PREFIX_SET=1; shift ;;
        *) die "未知参数：$1；使用 --help 查看用法" ;;
    esac
done
if [ "$PREFIX_SET" -eq 1 ]; then
    [ -n "$PREFIX" ] || die '--prefix 需要一个非空目录'
else
    [ -n "${HOME:-}" ] || die 'HOME 未设置；请用 --prefix DIR 指定安装目录'
    PREFIX=$HOME/.local
fi
case $PREFIX in /*) ;; *) PREFIX=$(pwd)/$PREFIX ;; esac
BIN=$PREFIX/bin
[ "$(uname -s)" = Darwin ] || die '只支持 macOS（Darwin）'
[ "$(uname -m)" = arm64 ] || die '只支持 Apple Silicon（arm64）'
command -v shasum >/dev/null 2>&1 || die '找不到 shasum，无法校验发行包'
SCRIPT_DIR=$(CDPATH= cd -P "$(dirname "$0")" && pwd) || die '无法定位脚本所在目录'
SUMS=$SCRIPT_DIR/SHA256SUMS
[ -f "$SUMS" ] || die '发行包不完整：找不到 SHA256SUMS'

expected_hash() {
    awk -v want="bin/$1" '
        {
            name = $0
            sub(/^[0-9A-Fa-f]+[[:space:]]+\*?/, "", name)
            sub(/^\.\//, "", name)
            if (name == want) { hash = tolower($1); count++ }
        }
        END {
            if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-f]/) exit 1
            print hash
        }
    ' "$SUMS"
}
sha_of() { shasum -a 256 "$1" | awk '{ print tolower($1) }'; }

# Check all CAYW executable names for this user, including a Client launched
# from a different prefix. Do not inspect private state or trust stale PID files.
# This is a quiescent-install check, not a lock against concurrent application launches.
check_stopped() {
    processes=$(ps -axww -o uid=,pid=,comm=) || die '无法检查运行中的进程；未替换程序'
    running=$(printf '%s\n' "$processes" | awk -v uid="$(id -u)" '
        $1 == uid {
            pid = $2
            sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+/, "")
            if ($0 ~ /(^|\/)(cayw|cayw-client|cayw-tui|cayw-webui)$/) print pid
        }')
    [ -z "$running" ] || die "检测到运行中的 CAYW（PID: $(printf '%s' "$running" | tr '\n' ' ')）。请正常退出并等待任务清理后重试；--force 不会停止任务"
}

for program in $PROGRAMS; do
    source=$SCRIPT_DIR/bin/$program
    [ -f "$source" ] && [ -x "$source" ] || die "发行包缺少可执行的 bin/$program"
    expected=$(expected_hash "$program") || die "SHA256SUMS 缺少或含有无效/重复的 bin/$program 记录"
    [ "$(sha_of "$source")" = "$expected" ] || die "SHA256 校验失败：bin/${program}；未改动任何已安装程序"
done
printf '校验通过：四个程序与 SHA256SUMS 一致。\n'
check_stopped
mkdir -p "$BIN" || die "无法创建 $BIN"
[ -w "$BIN" ] || die "没有写入 $BIN 的权限（脚本不使用 sudo）"
lock=$BIN/.cayw-install.lock
mkdir "$lock" 2>/dev/null || die "$lock 已存在或无法创建。可能有其他安装或未完成恢复；不要删除备份后盲目重试，请按安装指南核对"
TRANSACTION=$lock
chmod 700 "$TRANSACTION"
printf '%s\n' "$$" > "$TRANSACTION/installer.pid"

for program in $PROGRAMS; do
    dest=$BIN/$program
    if [ -e "$dest" ] || [ -L "$dest" ]; then
        [ -f "$dest" ] && [ ! -L "$dest" ] || die "$dest 不是普通文件或是符号链接，拒绝覆盖"
        if [ "$(sha_of "$dest")" != "$(expected_hash "$program")" ] && [ "$FORCE" -ne 1 ]; then
            die "$dest 已存在且与发行包不同；确认替换请加 --force（原程序未改动）"
        fi
        cp -p "$dest" "$TRANSACTION/old.$program" || die "无法备份 $dest"
    fi
    cp "$SCRIPT_DIR/bin/$program" "$TRANSACTION/new.$program" || die "无法暂存 $program"
    chmod 755 "$TRANSACTION/new.$program"
    [ "$(sha_of "$TRANSACTION/new.$program")" = "$(expected_hash "$program")" ] || die "复制后校验失败：$program"
    "$TRANSACTION/new.$program" --help >/dev/null 2>&1 || die "自检失败：$program --help；未改动任何程序"
done
check_stopped
# The persistent marker distinguishes a complete backup set from preparation.
# An uncatchable kill leaves this directory intact and blocks a later install.
printf 'complete original set saved; replacement may have started\n' > "$TRANSACTION/replacing"
REPLACING=1
for program in $PROGRAMS; do
    mv -f "$TRANSACTION/new.$program" "$BIN/$program" || die "无法写入 $BIN/$program"
done
for program in $PROGRAMS; do
    "$BIN/$program" --help >/dev/null 2>&1 || die "安装后自检失败：$program --help；正在恢复原程序"
done
REPLACING=0
printf '自检通过：四个程序的 --help 正常。\n'
for program in $PROGRAMS; do printf '已安装 %s：%s/%s\n' "$program" "$BIN" "$program"; done
case ":${PATH:-}:" in
    *":$BIN:"*) ;;
    *)
        printf '\n提示：%s 不在 PATH 中。请加入 shell 配置或执行：\n' "$BIN"
        printf '  export PATH="%s:$PATH"\n' "$BIN" ;;
esac
printf '\n下一步：\n'
printf '  cayw start --ui web --no-open  # 无模型也能启动并创建/查询会话\n'
printf '  cayw start --ui tui            # 启动终端界面\n'
printf '  cayw conf                      # 需要执行模型任务时，显式配置当前工作区\n'
printf '\n启动不会弹出配置向导。可选 api_key 明文保存在私有 models.json；公开状态不会回显密钥。\n'
