#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd -P)
wrapper=$root/examples/jit_support/rustc_repro.sh
test -x "$wrapper"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/compiler" <<'COMPILER'
#!/usr/bin/env bash
printf '%s\0' "$@" > "$LUNA_REPRO_ARGS"
COMPILER
chmod +x "$tmp/compiler"
export LUNA_REPRO_ARGS=$tmp/actual
export LUNA_REPRO_SOURCE="$tmp/source with spaces"
export LUNA_REPRO_TARGET="$tmp/source with spaces/target output"
"$wrapper" "$tmp/compiler" --crate-name probe 'source file.rs' -C opt-level=3
printf '%s\0' --crate-name probe 'source file.rs' -C opt-level=3 \
    "--remap-path-prefix=$LUNA_REPRO_SOURCE=luna-source" \
    "--remap-path-prefix=$LUNA_REPRO_TARGET=luna-build" > "$tmp/expected"
cmp "$tmp/expected" "$tmp/actual"
rm "$tmp/actual"
for mode in source-missing target-missing relative equals newline; do
    status=0
    (
        case "$mode" in
            source-missing) unset LUNA_REPRO_SOURCE;;
            target-missing) unset LUNA_REPRO_TARGET;;
            relative) export LUNA_REPRO_TARGET=relative;;
            equals) export LUNA_REPRO_SOURCE='/source=ambiguous';;
            newline) export LUNA_REPRO_TARGET=$'/target\nambiguous';;
        esac
        "$wrapper" "$tmp/compiler" probe
    ) > "$tmp/$mode.log" 2>&1 || status=$?
    test "$status" = 2
    test ! -e "$tmp/actual"
done
status=0
"$wrapper" > "$tmp/missing-compiler.log" 2>&1 || status=$?
test "$status" = 2
printf 'jit_repro_wrapper=passed argument_forwarding=exact invalid_inputs=6\n'
