#!/bin/sh
# Regenerate the test-only TLS fixtures in this directory (ca.pem, server.pem,
# server.key.pem). What they are, why they are harmless, and the rule that
# they are never used outside the test suite: see README.md alongside.
#
#   sh crates/mu-irc-gateway/tests/fixtures/make-tls-fixtures.sh
#
# The CA key is created in a temp directory that the trap below removes; only
# the CA certificate, the leaf, and the leaf's key land next to this script.
#
# P-256/SHA-256 rather than Ed25519 so the fixture verifies under every rustls
# crypto provider, not just the one this crate happens to pin today.
set -eu

dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

days=3650

openssl ecparam -name prime256v1 -genkey -noout -out "$work/ca.key"
openssl req -x509 -new -key "$work/ca.key" -sha256 -days "$days" \
    -subj "/CN=mu-irc-gateway test CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$dir/ca.pem"

openssl ecparam -name prime256v1 -genkey -noout -out "$work/leaf.key"
openssl pkcs8 -topk8 -nocrypt -in "$work/leaf.key" -out "$dir/server.key.pem"
openssl req -new -key "$dir/server.key.pem" \
    -subj "/CN=irc.test.invalid" -out "$work/leaf.csr"

cat > "$work/leaf.ext" <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=IP:127.0.0.1,DNS:irc.test.invalid
EXT

openssl x509 -req -in "$work/leaf.csr" -sha256 -days "$days" \
    -CA "$dir/ca.pem" -CAkey "$work/ca.key" -set_serial 1 \
    -extfile "$work/leaf.ext" \
    -out "$dir/server.pem"

openssl verify -CAfile "$dir/ca.pem" "$dir/server.pem"
