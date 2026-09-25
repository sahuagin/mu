#!/bin/sh
# Regenerate the test-only TLS fixtures in this directory: the server side
# (ca.pem, server.pem, server.key.pem) and two SLOT client credentials
# (slot-a.pem/.key.pem, slot-b.pem/.key.pem). What they are, why they are
# harmless, and the rule that they are never used outside the test suite: see
# README.md alongside.
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

# Two SLOT credentials. Self-signed and CA-less on purpose: a server's certfp
# matches a FINGERPRINT, not a chain, so this is the shape a real slot account
# is provisioned with. They must be DISTINCT from each other — the config
# refuses a pool that files one certificate under two accounts, because one
# fingerprint maps to one account, and a test proves it.
for slot in a b; do
    openssl ecparam -name prime256v1 -genkey -noout -out "$work/slot-$slot.key"
    openssl pkcs8 -topk8 -nocrypt -in "$work/slot-$slot.key" \
        -out "$dir/slot-$slot.key.pem"
    openssl req -x509 -new -key "$dir/slot-$slot.key.pem" -sha256 -days "$days" \
        -subj "/CN=mu-irc-gateway test slot $slot" \
        -addext "basicConstraints=critical,CA:FALSE" \
        -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=clientAuth" \
        -out "$dir/slot-$slot.pem"
done

# The whole point of having two is that they differ.
if [ "$(openssl x509 -in "$dir/slot-a.pem" -noout -fingerprint -sha256)" = \
     "$(openssl x509 -in "$dir/slot-b.pem" -noout -fingerprint -sha256)" ]; then
    echo "slot-a and slot-b must not share a fingerprint" >&2
    exit 1
fi
