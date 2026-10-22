#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'Oslo consumer: %s\n' "$*" >&2; exit 2; }
[[ $# == 3 ]] || fail 'expected setup|build|check|capture, fresh directory, Oslo checkout'
stage=$1
root=$2
oslo=$3
tools=$(cd "$(dirname "$0")" && pwd -P)
luna=$(cd "$tools/../.." && pwd -P)
revision=f61ccc5d97ded403cea0edc63ace82b8834f7bd6
[[ $root == /* && $oslo == /* && $root != *$'\n'* && $root != *'='* ]] || fail 'absolute unambiguous paths required'
[[ $root != "$luna" && $root != "$oslo" ]] || fail 'evidence must not replace either checkout'
files=(oslo_consumer.sh oslo_consumer.patch oslo_consumer_run.pl oslo_consumer_audit.pl)

verify() {
    (cd "$root"; sha256sum -c tools.sha256 source.sha256 >"$root/verification-$stage.log")
    for file in "${files[@]}"; do cmp "$tools/$file" "$root/tools/$file"; done
}

case $stage in
setup)
    git -C "$luna" diff --quiet -- src derive util Cargo.toml Cargo.lock || fail 'commit runtime edits first'
    git -C "$luna" diff --cached --quiet -- src derive util Cargo.toml Cargo.lock || fail 'commit staged runtime edits first'
    git -C "$oslo" cat-file -e "$revision^{commit}" || fail 'required Oslo revision is unavailable'
    mkdir "$root"
    mkdir "$root/luna" "$root/oslo" "$root/tools" "$root/images" "$root/work" "$root/home"
    git -C "$luna" rev-parse HEAD >"$root/luna-head"
    printf '%s\n' "$revision" >"$root/oslo-head"
    git -C "$luna" archive "$(cat "$root/luna-head")" | tar -C "$root/luna" -xf -
    git -C "$oslo" archive "$revision" | tar -C "$root/oslo" -xf -
    cp "$root/oslo/Cargo.lock" "$root/oslo-original.lock"
    for file in "${files[@]}"; do cp "$tools/$file" "$root/tools/$file"; done
    patch --batch --fuzz=0 -p1 -d "$root/oslo" <"$root/tools/oslo_consumer.patch" >"$root/adapter.log"
    cd "$root/oslo"
    cargo metadata --format-version=1 --features oslo-luavm/luna-jit-bench >"$root/metadata-initial.json"
    awk '/^\[\[package\]\]/{name=""} /^name = "cranelift-/{name=$3;gsub(/"/,"",name)} /^version = / && name!="" {v=$3;gsub(/"/,"",v);print name "\t" v}' "$root/luna/Cargo.lock" | sort >"$root/backend-expected.tsv"
    for name in cranelift-codegen-meta cranelift-assembler-x64 cranelift-assembler-x64-meta cranelift-control cranelift-codegen-shared cranelift-srcgen; do
        version=$(awk -v name="$name" '$1==name {print $2}' "$root/backend-expected.tsv")
        [[ -n $version && $version != *$'\n'* ]] || fail 'ambiguous backend pin'
        cargo update --offline -p "$name" --precise "$version" >>"$root/backend-resolution.log" 2>&1
    done
    cargo metadata --locked --format-version=1 --features oslo-luavm/luna-jit-bench >"$root/metadata.json"
    jq -r '.packages[]|select(.name|startswith("cranelift-"))|[.name,.version]|@tsv' "$root/metadata.json" | sort >"$root/backend-actual.tsv"
    cmp "$root/backend-expected.tsv" "$root/backend-actual.tsv"
    cd "$root"
    printf 'protocol=oslo-luna-consumer-v1\nprofiles=3,s\nrows=10000\nsamples=11\nwarmups=2\nrounds=6\ncpus=0,16\nthreshold=unscored\n' >configuration
    sha256sum tools/* >tools.sha256
    find luna oslo -type f -print0 | sort -z | xargs -0 sha256sum >source.sha256
    perl -MJSON::PP -e 'print JSON::PP->new->canonical->encode([map {{id=>$_,free=>($_%2 ? 1500000000 : 500000000),mount=>"/mnt/volume-".($_%8),used=>123456789+$_}} 0..9999])' >work/rows.json
    perl -MJSON::PP -e 'print JSON::PP->new->canonical->encode([map {{id=>$_,free=>(999999999,1000000000,1000000001)[$_%3],mount=>"/mnt/boundary",used=>123456789+$_}} 0..9999])' >work/boundary.json
    sha256sum work/rows.json work/boundary.json >fixture.sha256
    ;;
build)
    verify
    cd "$root/oslo"
    for opt in 3 s; do
        export CARGO_TARGET_DIR="$root/cache-$opt" CARGO_PROFILE_RELEASE_OPT_LEVEL=$opt
        export CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true
        mkdir "$root/images/$opt"
        cargo tree --locked -p oslo -e normal >"$root/images/$opt/no-jit-dependencies.log"
        ! grep -q cranelift "$root/images/$opt/no-jit-dependencies.log" || fail 'default graph contains compiler'
        cargo build --locked --release -p oslo
        cp "$CARGO_TARGET_DIR/release/oslo" "$root/images/$opt/no-jit"
        cargo build --locked --release -p oslo --features oslo-luavm/luna-jit-bench
        cp "$CARGO_TARGET_DIR/release/oslo" "$root/images/$opt/jit"
    done
    cd "$root"
    sha256sum images/{3,s}/{no-jit,jit} >images.sha256
    verify
    ;;
check|capture)
    verify
    cd "$root"
    sha256sum -c images.sha256 fixture.sha256
    mkdir "$root/$stage"
    perl "$root/tools/oslo_consumer_run.pl" "$root" "$stage"
    perl "$root/tools/oslo_consumer_audit.pl" "$root" "$stage" >"$root/$stage/verified.json"
    sha256sum -c images.sha256 fixture.sha256
    verify
    ;;
*) fail 'unknown stage';;
esac
