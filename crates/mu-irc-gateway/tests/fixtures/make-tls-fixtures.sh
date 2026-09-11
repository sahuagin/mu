#!/bin/sh
# Regenerate the private-CA TLS fixtures used by the offline transport tests.
#
# The fixtures are COMMITTED, not generated at test time: `rcgen` is not in this
# workspace's dependency graph (checked against Cargo.lock), and adding a
# certificate-generation crate to build a two-certificate chain is a larger
# dependency decision than the test needs. They are valid for 10 years from the
# date below; re-run this script to replace them.
#
#   sh crates/mu-irc-gateway/tests/fixtures/make-tls-fixtures.sh
#
# What lands in git:
#
#   ca.pem          the test CA certificate — the PEM bundle a test feeds to
#                   `[irc] tls_ca_file`
#   server.pem      a leaf certificate signed by that CA, with SANs
#                   `IP:127.0.0.1` and `DNS:irc.test.invalid`
#   server.key.pem  the leaf's PKCS#8 private key
#
# The CA's PRIVATE key is deliberately NOT kept: nothing in the tests signs
# anything, so the only thing a retained CA key could do is sign something else.
# The leaf key has to be kept — the test's TLS server presents it — and is a
# throwaway that has never protected anything.
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
