set -eu -o pipefail

root=$1
evidence=$2
test ! -e "$evidence"
mkdir -p "$evidence"
source_dir=$(mktemp -d /tmp/luna-package-source-XXXXXX)
printf '%s\n' "$source_dir" >"$evidence/source-path"
git -C "$root" rev-parse HEAD >"$evidence/base-commit"
git -C "$root" diff --binary HEAD >"$evidence/source.patch"
git -C "$root" ls-files -z | tar -C "$root" --null -T - -cf - | tar -C "$source_dir" -xf -
test -f "$source_dir/tests/release_consumer/src/main.rs"
test ! -e "$source_dir/.git"
cd "$source_dir"
find . -type f -print0 | sort -z | xargs -0 sha256sum >"$evidence/source.sha256"
rustc -vV >"$evidence/rustc.txt"
cargo -V >"$evidence/cargo.txt"
cargo vendor --locked --offline --versioned-dirs "$evidence/vendor" >"$evidence/vendor-config.toml"
export CARGO_TARGET_DIR="$evidence/package-target"

stage_package() {
    local name=$1
    local archive="$evidence/package-target/package/$name-0.5.1.crate"
    local destination="$evidence/vendor/$name-0.5.1"
    test ! -e "$destination"
    tar -xzf "$archive" -C "$evidence/vendor"
    tar -tzf "$archive" >"$evidence/$name-files.txt"
    local checksum
    checksum=$(sha256sum "$archive" | cut -d' ' -f1)
    (cd "$destination"; find . -type f ! -name .cargo-checksum.json -print0 | sort -z | xargs -0 sha256sum --zero) |
        jq -Rs --arg package "$checksum" 'split("\u0000") | map(select(length>0) | capture("^(?<hash>[0-9a-f]{64})  \\./(?<name>.*)$";"s") | {key:.name,value:.hash}) | {package:$package,files:from_entries}' >"$destination/.cargo-checksum.json"
}

for package in luna-derive luna-jit-embed; do
    cargo package --locked --offline --allow-dirty --no-verify -p "$package"
    stage_package "$package"
done
for package in luna luna-util; do
    cargo --config 'source.crates-io.replace-with="package-smoke"' \
        --config "source.package-smoke.directory=\"$evidence/vendor\"" \
        package --locked --offline --allow-dirty --no-verify -p "$package"
    stage_package "$package"
done
sha256sum "$evidence"/package-target/package/*.crate >"$evidence/archives.sha256"
sha256sum -c "$evidence/source.sha256" >"$evidence/source-after-package.txt"
mkdir "$evidence/cargo-home"
printf '[source.crates-io]\nreplace-with="package-smoke"\n[source.package-smoke]\ndirectory="%s/vendor"\n[net]\noffline=true\n' "$evidence" >"$evidence/cargo-home/config.toml"
export CARGO_HOME="$evidence/cargo-home"
export CARGO_TARGET_DIR="$evidence/consumer-target"
cp -a "$source_dir/tests/release_consumer" "$evidence/consumer"
cd "$evidence/consumer"
cargo generate-lockfile --offline
for feature in nojit jit; do
    flags=()
    if [[ $feature == jit ]]; then flags=(--features jit); fi
    cargo tree --locked --offline --edges normal "${flags[@]}" >"$evidence/$feature.tree"
    cargo metadata --locked --offline --format-version 1 "${flags[@]}" >"$evidence/$feature.metadata.json"
    jq -e --arg prefix "$evidence/vendor/" --arg mode "$feature" '
        [.packages[] | select(.name != "luna-release-probe")] as $dependencies |
        all($dependencies[]; .source == "registry+https://github.com/rust-lang/crates.io-index" and (.manifest_path | startswith($prefix))) and
        all($dependencies[]; .name != "luna-jit-embed" and .name != "sha2") and
        (any($dependencies[]; .name == "cranelift-codegen") == ($mode == "jit")) and
        all($dependencies[] | select(.name | test("^luna($|-)")); .version == "0.5.1")
    ' "$evidence/$feature.metadata.json" >"$evidence/$feature-isolation.txt"
    cargo build --locked --offline --release "${flags[@]}"
    cp "$CARGO_TARGET_DIR/release/luna-release-probe" "$evidence/consumer-$feature"
done
for lane in nojit-off jit-off jit-auto; do
    feature=${lane%-*}
    mode=${lane##*-}
    env -i PATH=/nonexistent "$evidence/consumer-$feature" "$mode" >"$evidence/$lane.txt"
    compiled=0
    if [[ $feature == jit ]]; then compiled=1; fi
    test "$(grep -c '^case=' "$evidence/$lane.txt")" = 7
    grep -Fx "release_probe=1 jit=$compiled mode=$mode cases=7 derive=1 serde=1 async_wakes=2" "$evidence/$lane.txt"
done
sha256sum "$evidence/consumer-nojit" "$evidence/consumer-jit" >"$evidence/images.sha256"
sha256sum -c "$evidence/archives.sha256" "$evidence/images.sha256" >"$evidence/artifacts-after.txt"
cd "$source_dir"
sha256sum -c "$evidence/source.sha256" >"$evidence/source-after.txt"
printf 'package_smoke=1 lanes=3 cases=21 upload=0 performance_acceptance=0\n'
