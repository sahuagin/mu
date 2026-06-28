# RECOMMENDED COMPOSITION — mu inter-agent messaging substrate

| field   | value                                                                                             |
| ------- | ------------------------------------------------------------------------------------------------- |
| spec_id | architecture/messaging-substrate-composition                                                      |
| status  | draft                                                                                             |
| created | 2026-06-28                                                                                        |
| authors | tcovert + cc (claude-opus-4.8)                                                                     |
| bead    | mu-q0oe (epic) · landed via mu-bsr1                                                                |
| related | mu-001 (protocol-types — demoted to edge), architecture/mu-capability-substrate, mu-slat, mu-vf0z |
| sources | research/messaging-substrate-prior-art.md (+ .json) — verified prior-art survey F1–F11             |

> The tiered stack the stalled research run never emitted. Built strictly on the verified prior art
> (findings F1–F11 in `research/messaging-substrate-prior-art.md`; raw claims/sources in
> `research/messaging-substrate-prior-art.json`). Where a pick goes
> beyond the verified set it is tagged **REASONED-UNVERIFIED** inline. The architecture (L1–L4 + MCP-as-edge)
> is settled; this designs *to* it, it does not relitigate it.
>
> **Operator decision (folded in):** the fleet is **model-latency-bound, not wire-throughput-bound** —
> aggregate message rate is gated by per-turn LLM latency (seconds), so body serialization cost (microseconds)
> is ~6 orders of magnitude from mattering. Therefore **L4 defaults to JSON/CBOR**; SBE/Cap'n Proto are
> **measured per-type upgrades**, not the default, and **rkyv is deferred**. Strictness is a **validation
> property** (bodies validated against the L3 catalog on ingest), not a wire guarantee — *typed ≠ binary*. This
> reframes the substrate's value as **correctness + evolvability** (typing, versioning, routing, MCP-demotion),
> which never depended on a binary wire, rather than speed, which the model latency makes moot today.

---

## 1. Headline recommendation

Compose, don't adopt — no shipped stack lands all three pillars without HTTP/2 + protobuf. Run a single
transport-agnostic **L2 envelope** (SBE-header-shaped: `templateId` = msg-type, `stride` = total-body
length-to-skip, `schemaId`+`version` = schema-version, plus `src/dst/correlationId` routing and an optional
trailer CRC/HMAC) that the **router reads exclusively and forwards on alone**, over a **tiered L1 transport**
— **Aeron IPC** same-host, **Aeron UDP unicast/multicast** on owned LANs, and **QUIC- or TLS/TCP-framed**
links the moment a stream leaves boxes you own (middlebox ossification: only TCP/UDP survive, F2). **L3
typed discovery** is a per-stream two-phase handshake (Arrow-Flight-shaped `GetFlightInfo→DoGet`, F9, minus
gRPC/protobuf/HTTP-2) that negotiates a **numeric template-id catalog in an mu-owned `schemaId` namespace**
**once per stream, not per message**, with a **gRPC-reflection-style runtime descriptor fetch** (F6) so a
peer behind on versions pulls the strict type and converts locally; **Smithy is the authoring source of
truth** (F7) feeding a hand-written SBE binding (because rpcv2Cbor is HTTP-POST + name-keyed, F8). **L4 is
pluggable and opaque to the router**, and — because the fleet is model-latency-bound — **defaults to
JSON/CBOR** (validated against the L3 catalog on ingest, which is what keeps it strictly typed); **SBE** (F3)
and **Cap'n Proto** (F4) are **measured per-type upgrades**, plugged in only where a non-LLM-gated path is shown
to need the throughput or zero-copy/capabilities, never by default, with **rkyv deferred**. **MCP is a thin edge adapter** — a slimy-FIX shim
that terminates JSON-RPC and absorbs the impedance on our side, civilized at the edge, feral internally —
and BEEP's fate (F11) is the standing warning that keeps that edge door familiar.

**Layer picks at a glance:**
- **L1 transport:** Aeron IPC (same-host) → Aeron UDP unicast+multicast (owned LAN) → QUIC / TLS-TCP (off-LAN). [ZeroMQ/nng/NATS/iceoryx = reasoned fallbacks]
- **L2 envelope:** bespoke SBE-header-shaped router-only frame; `stride` = whole-body skip length (SOFH role).
- **L3 typed discovery:** per-stream handshake (Flight two-phase shape) negotiating an mu-owned numeric `schemaId`/`templateId` catalog; gRPC-reflection-style fetch-and-convert; Smithy IDL as source of truth → custom SBE binding.
- **L4 body codec:** **JSON/CBOR by default** (fleet is model-latency-bound), with **validate-on-ingest** against the L3 catalog for strictness; SBE / Cap'n Proto as **measured per-type upgrades**; rkyv deferred.
- **xkcd-927 verdict:** No single existing stack composes lean-transport + numeric-template-id-skip-by-length + transport-agnostic-typed-discovery without dragging in HTTP/2 + protobuf — so compose, stealing SBE (F3), Cap'n Proto (F4), gRPC-reflection (F6), Smithy (F7/F8), Flight's two-phase (F9), Avro's fingerprint-ID (F5), and BEEP's reusable-session idea (F11).

---

## 2. Layer by layer

### L1 — Transport (tiered; the envelope rides unchanged across all tiers)

The envelope (L2) is the invariant; transport is a swappable substrate beneath it. That separation *is*
BEEP's reusable-session steal (F11) and SBE's transport-independence — the same frame must ride shared
memory, multicast, and a TLS socket without re-encoding. Tier by where the stream physically goes:

**Tier 0 — same-host IPC (agents co-located on one box).**
Pick: **Aeron IPC** (shared-memory log buffers) — **VERIFIED F1**: Aeron delivers reliable UDP unicast,
UDP multicast, *and* same-machine IPC in one library, language-agnostic at the client seam (first-class
Java/C/C++11/.NET; canonical client is JVM, native path is C/C++). This is the LMAX/Disruptor-adjacent path
the operator already thinks in. For *pinned same-version same-language Rust* peers there is an even leaner
option — a shared-memory ring carrying `repr(C)`/rkyv bodies (see §6) — but that is a codec/tier choice
negotiated at handshake, not a different transport. **iceoryx** (zero-copy shm IPC) is the obvious
alternative here but **REASONED-UNVERIFIED** (it was fetched into the run's sources but no claim survived to
the verified set — see report caveat on the degenerate transport pillar).

**Tier 1 — owned LAN (the fleet's normal operating environment).**
Pick: **Aeron UDP unicast for point-to-point, Aeron UDP multicast for fan-out** — **VERIFIED F1**.
Multicast maps directly onto the operator's prior RTI-DDS reliable-multicast market-data work: one publisher,
N agent subscribers, no broker in the hot path. "Local-network UDP acceptable" in the settled architecture is
satisfied natively here; this is the lean alternative to a full DDS that F1 explicitly positions Aeron as.
**ZeroMQ** is the operator's stated default and the pragmatic brokerless fallback, but it is
**REASONED-UNVERIFIED** — the report is explicit that ZeroMQ "has no surviving verified claim," so treat its
reliability/persistence gaps versus Aeron as an open bake-off (report Open Q2), not a settled win. **nng**
(Sústrik→D'Amore lineage) sits in the same reasoned bucket. **SCTP** belongs *only* on this tier — its
message-orientation and multistreaming are attractive, but **VERIFIED F2** confirms middlebox ossification
confined it to controlled LANs; use it only where you own every box between endpoints, if at all.

**Tier 2 — cross-boundary / uncontrolled network (off-LAN, NAT/firewall in path).**
Constraint: **VERIFIED F2** — firewalls/NATs/routers reliably pass only TCP or UDP, so anything leaving owned
boxes must be TCP- or UDP-framed. Picks: **QUIC** (rides UDP, TLS 1.3 built in, no head-of-line blocking) is
"the SCTP that survives networks you don't control"; reach for it when you want SCTP's multistreaming without
SCTP's death-by-middlebox. **Caveat / REASONED-UNVERIFIED:** QUIC's usability *standalone, outside HTTP/3*
is the run's Open Q4 — unconfirmed; until proven, the safe cross-boundary default is **TLS/TCP**. **NATS
(+JetStream)** is the brokered, persistence-bearing option for store-and-forward across the boundary, also
**REASONED-UNVERIFIED**. The MCP edge adapter (§4) also lives on this tier, over HTTP+SSE / stdio.

The router never re-frames bodies across tiers; it reads the envelope and re-publishes onto the next tier's
transport. The envelope's transport-independence is what makes a Tier-0 IPC message and a Tier-2 QUIC message
routable by one code path.

### L2 — Envelope (the router's *only* input; body is opaque)

Pick: a **bespoke envelope shaped like the SBE message header, extended with routing and integrity.** This is
where **VERIFIED F3** is load-bearing: SBE's `blockLength` *is* the stride/length-to-skip, and the operator's
independently re-derived mapping (`blockLength`=stride, `templateId`=type, `schemaId`+`version`=schema-version)
is confirmed verbatim against the real-logic SBE wiki. The envelope fields map:

| Settled envelope field | Concrete encoding | Source-of-shape |
|---|---|---|
| msg-type | `templateId` (uint16) | SBE header (F3) |
| stride (length-to-skip) | **whole-body byte length** (uint32) — see precision note | SOFH role; SBE `blockLength` is the *intra-body* analog (F3) |
| schema-version | `schemaId` (uint16, mu-owned namespace) + `version` (uint16) | SBE header (F3) |
| routing | `src` / `dst` / `correlationId` (fixed-width ids) | settled architecture |
| optional integrity | trailing CRC-32C or HMAC over envelope+body | settled architecture; CRC idiom per Avro/SBE framing |

**Precision note (REASONED, goes beyond F3 — flagged):** SBE's `blockLength` covers the *root block only*,
not repeating groups or var-data; whole-message framing in the SBE world is the separate SOFH's
(Simple Open Framing Header) job. The router needs to skip the *entire* unknown body, so the **envelope
`stride` must be a whole-body length (SOFH role), not raw `blockLength`.** There are therefore two strides at
two layers: the **outer envelope `stride`** lets the *router* skip an opaque/unknown/newer body wholesale; the
**inner SBE `blockLength`** (inside L4) lets a *typed decoder* skip appended fields for version tolerance
(F3). The router only ever touches the outer one. This is exactly the nuance the run's verifiers flagged when
they noted `blockLength` is root-block-scoped — I am resolving it, not contradicting F3.

How a router skips an unknown/newer body: it reads the fixed-width envelope, finds `templateId`/`schemaId`/
`version` it doesn't recognize (or a `dst` it must forward verbatim), and advances the read cursor by
`stride` bytes — never parsing L4. That is the mixed-version skip model (F3) lifted to the router. Integrity
rides as an optional trailer so a router that *does* verify can, and one that just forwards needn't.

### L3 — Typed schema / discovery (per-stream handshake, owned numeric namespace)

Pick: a **per-stream two-phase handshake** that negotiates the numeric `templateId` catalog **once at stream
open**, plus a **runtime descriptor-fetch** for peers that are behind. Composed from three verified pieces:

- **Per-stream, two-phase shape — Arrow Flight (VERIFIED F9).** Flight's `GetFlightInfo(FlightDescriptor)`→
  `DoGet(Ticket)` establishes schema *per stream, fetched once up front, not per message*, and ships built-in
  typed-discovery RPCs (`ListFlights`/`ListActions`/`GetSchema`). That is precisely mu's handshake template.
  We take the *shape* and drop the substrate: **VERIFIED F10** confirms Flight rides gRPC + protobuf +
  HTTP/2, which fails the lean-transport and protobuf-rejection constraints — so mu's HELLO is the
  Flight two-phase pattern carried in our own envelope over Aeron/QUIC, not Flight itself.
- **Runtime fetch-and-convert — gRPC server reflection (VERIFIED F6).** Reflection lets a client build
  requests at runtime with no precompiled stub: the server exports its *strict machine-readable descriptor
  database* over a standardized RPC, and the client encodes/decodes against it locally. That is mu's
  "caller fetches the up-to-date strict type and converts locally" exactly. Implementation: a peer that
  receives `schemaId/version` it lacks calls a reflection-style `GetCatalog(schemaId, version)` against the
  producer (or a registry) and gets the strict SBE descriptor (schema XML / compiled IR), then converts
  locally. Strict, not stringly.
- **Source-of-truth contract — Smithy (VERIFIED F7), with its own binding (VERIFIED F8).** Smithy is the
  strongest protocol/transport/language-agnostic IDL — "strict contract held separate from transport," codegen
  for any language. Author the message catalog *in Smithy* and generate the multi-language stubs from it. But
  **do not ship rpcv2Cbor**: F8 confirms it is nailed to HTTP POST and keys structs/unions *by member name*
  (text keys on the wire), with no numeric-template-id / skip-by-length frame. So the AWS-blessed wire is the
  wrong wire. We **author a custom Smithy protocol binding that emits SBE templates** (numeric `templateId`,
  `blockLength` stride, append-only). Smithy gives us the contract and the polyglot stubs; SBE gives us the
  wire. F8 is the explicit licence for "author your own binding."
- **What we steal from Avro/Confluent but reject — fingerprint vs per-stream (VERIFIED F5).** Avro's
  single-object encoding (2-byte magic + 8-byte CRC-64 schema fingerprint + body) and Confluent's compact
  schema-**ID**-on-the-wire solve "schema repeated per message" out-of-band — but that is schema-*reference*-
  per-message, explicitly *not* schema-per-stream-at-handshake. **Steal the fingerprint idea for the
  HELLO**: have the handshake carry a compact catalog fingerprint so peers detect version skew cheaply and
  trigger a `GetCatalog` fetch. **Reject the per-message reference**: mu binds the catalog *once per stream*,
  so the per-message bytes carry only the numeric `templateId`, never a schema reference. F5 is the precise
  line we are drawing.

### L4 — Body codec (pluggable per message-type, opaque to the router)

The router never reads L4 — that's the seam that lets the typed core and the JSON edge coexist behind one
envelope. **typed ≠ binary:** the type strictness lives in the L3 catalog + handshake, not in the wire format,
so the default codec is chosen for *fit to the current bottleneck*, not raw speed. The fleet is
**model-latency-bound** (a per-turn LLM call gates every message), so a binary body buys nothing on the hot
path today. Codecs, by use:

- **JSON / CBOR — the default (operator decision).** Self-describing, zero codegen, fast enough by ~6 orders
  of magnitude when a model turn gates each message. Default body codec for the core *and* the edge — not just
  behind the MCP adapter. **The tax it levies is discipline, not performance:** JSON will happily encode an
  off-schema message, so **strictness becomes a validation responsibility** — every decode boundary (and the
  MCP adapter) **MUST validate the body against its L3 catalog entry on ingest**, or the substrate quietly
  loses the "strict" property it exists to provide. That validate-on-ingest gate is a standing rule (§3), not
  an afterthought.
- **SBE — measured per-type upgrade (VERIFIED F3).** Numeric `templateId`, `blockLength` intra-body stride,
  append-only evolution, bounded-read forward/backward compat. SBE still **shapes the L2 envelope** (F3 is
  load-bearing *there*); as a *body* codec it is plugged in for a specific `templateId` only when a measured,
  non-LLM-gated path needs the throughput. Not the default.
- **Cap'n Proto — measured per-type upgrade (VERIFIED F4).** Where wire==memory random access matters (large
  structured payloads by reference, no parse step) or where you want **capability-passing between agents** —
  Cap'n Proto bundles capability-based RPC with the format. Reach for it per-type for capability grants and big
  zero-copy structs; opaque to the router like any other body.
- **rkyv / `repr(C)` — deferred (operator decision; was REASONED-UNVERIFIED).** Same-version/same-language/
  same-arch Rust↔Rust only; ABI/endianness-locked and version-fragile — the most fragile pick for the least
  current benefit. Deferred until a measured intra-Rust high-rate path justifies a second codec (§6, §8).

The codec in force is declared in the handshake (a `codecId` alongside `schemaId`/`version`), so the router
stays codec-blind and peers negotiate the strongest codec they *share* — which today defaults to JSON/CBOR and
rises to a binary codec only where both peers advertise it for a measured-hot `templateId`.

---

## 3. Schema / template-id lifecycle

**Where the catalog lives.** The catalog is a set of typed message definitions identified by an mu-owned
`schemaId` (claim a block of `schemaId`s for the fleet so sub-domains get their own); within a schema each
message-type is a numeric `templateId`. The catalog is **codec-independent** — it defines the *types*, not the
wire. The authoritative model is **Smithy IDL in a versioned repo** (F7), from which we generate the polyglot
stubs **and the validation schema for the JSON/CBOR default**, plus SBE descriptors via the custom binding
(F8) for any `templateId` upgraded to SBE. A **runtime descriptor service** (gRPC-reflection-analog, F6) serves
the strict definition for any `(schemaId, version)` on demand, so a peer behind on versions fetches it and
validates/decodes locally.

**How it's negotiated — once per stream, not per message.** On stream open, the two-phase HELLO (Flight
shape, F9) exchanges `schemaId`, `version`, a compact catalog **fingerprint** (Avro-idea steal, F5), and the
agreed `codecId`. After that, per-message wire bytes carry only the numeric `templateId` in the envelope —
the catalog is bound for the life of the stream. This is the explicit win over Avro/Confluent's per-message
schema-reference (F5).

**Validate-on-ingest (standing rule, forced by the JSON default).** Because the default body codec is
self-describing JSON/CBOR rather than a schema-bound binary, nothing on the *wire* prevents an off-schema body.
Every decode boundary — the receiving agent's codec layer and the MCP edge adapter (§4) — **MUST validate each
body against its `(schemaId, version, templateId)` catalog entry on ingest**, rejecting or quarantining
non-conforming messages. This gate is what makes "strictly typed" true at runtime: an SBE-upgraded type gets it
for free (it cannot encode off-schema), a JSON type gets it *only* because the validator enforces it. A missing
or lax gate silently reintroduces the untyped looseness the substrate exists to remove — treat a validation gap
as a correctness bug, not a nicety.

**How mixed versions coexist — append-only + skip-by-stride (F3).** Two mechanisms, two scopes:
1. *Within a known message-type (field-level evolution) — codec-dependent.* For the **JSON/CBOR default**,
   tolerance is the validator's job: ignore-unknown fields, default-missing fields per the catalog version, at
   the validate-on-ingest gate. For an **SBE-upgraded type** (F3) it's append-only — new fields at the end of
   the block, `blockLength` grows; an old decoder reads up to the old `blockLength` and null-fills absent
   fields, a new decoder reading old data stops at the old `blockLength` (bounded-read forward/backward compat).
   Either way the field-level tolerance is an L4 concern, invisible to the router.
2. *Across unknown message-types:* a peer that doesn't know `templateId N` (a brand-new type, or a newer
   `version`) skips the **whole frame by the envelope `stride`** — the router does this without ever
   consulting L4. Old receivers skip unknown messages by length, exactly as the settled architecture states.

**Rollout without a flag day.** Adding a field → append to the SBE template, bump `version`; producers
advertise the new `version` in HELLO, consumers that lack it either fetch the descriptor (F6) and convert
locally or read bounded to their known `blockLength`. Adding a message-type → allocate a new `templateId`;
peers that don't know it skip by `stride`. New and old peers interoperate continuously; no synchronized
upgrade. This is the SBE mixed-version model (F3) operating at both the field and the message-type scope.

---

## 4. MCP as edge adapter (the slimy-FIX shim)

**The seam.** A single **envelope-speaking adapter process** terminates MCP (JSON-RPC over stdio / HTTP+SSE)
on the outside and speaks the typed core (envelope + SBE/Cap'n-Proto bodies over Aeron/QUIC) on the inside.
It sits on Tier-2 transport. The typed feral core never sees JSON-RPC; the LCD MCP peers never see SBE.

**What the adapter translates:**
- *Message bodies:* MCP tool-call JSON ⇄ a typed `templateId` SBE message. JSON→strict on the way in
  (validating against the catalog), strict→JSON on the way out.
- *Discovery:* MCP introspection (`tools/list`) ⇄ the L3 typed catalog. The adapter holds the catalog and
  renders it as MCP tool schemas outward, and resolves MCP tool names to numeric `templateId`s inward. This
  is the down-conversion of the strict numeric-template catalog (F6/F9 territory) to MCP's stringly surface.
- *Routing:* maps MCP request/response correlation to the envelope's `correlationId`.

**Why this is the slimy-FIX move, not rebuilt lock-in.** You pay the JSON↔typed integration cost **once, at
the adapter, on your side** — keeping old/foreign MCP consumers alive without letting JSON-RPC's untyped
looseness leak into the core, exactly as a FIX shim keeps legacy consumers alive while the modern path stays
binary. Civilized at the edge, feral internally. **BEEP's fate (F11) is the discipline here:** BEEP was a
correct-but-obscure reusable-session framework that lost to "just use HTTP" because integrators default to
the familiar, ubiquitous, firewall-friendly option. The lesson is *not* "don't build the typed core" — it is
"don't force every integrator to implement your obscure stack to talk to you." MCP-over-HTTP/stdio is the
familiar door; we absorb the impedance behind it rather than exporting it. That is how you get BEEP's
reusable-session benefit (F11) without BEEP's adoption death.

---

## 5. xkcd-927 verdict

**No single existing stack composes all three pillars — lean transport + numeric-template-id skip-by-length
typed format + transport-agnostic typed discovery — without dragging in HTTP/2 + protobuf.** Compose.

What each candidate *is* and the baggage that disqualifies adopting it whole:

- **gRPC / Arrow Flight** — typed + real runtime discovery (F6/F9), but **rides HTTP/2 + protobuf** (F10):
  fails lean-transport and the operator's protobuf rejection. The LCD/web trap again.
- **DDS** — genuinely composes transport + typed format + discovery + QoS in one, but heavy (operator's
  standing judgment; open DDS impls Cyclone/OpenDDS/FastDDS are **REASONED-UNVERIFIED** here — the run's
  transport pillar degenerated before they were verified). Steal the *ideas* (content-filtered subscriptions,
  discovery, QoS), not the stack.
- **Smithy + a wire** — Smithy IDL is the right contract shape (F7), but it has **no blessed numeric-
  template-id skip-by-length binding**: rpcv2Cbor is HTTP-POST + name-keyed (F8). You must author the binding.
- **Cap'n Proto** — format + capability RPC in one (F4), but it is not a numeric-template skip-by-stride
  envelope and carries no tiered-transport or discovery story. A pillar, not the stack.

**What we steal from each (the compose list):**
- **SBE (F3):** the envelope header shape and the append-only / skip-by-length versioning model — L2 + L4 hot path.
- **Cap'n Proto (F4):** zero-copy bodies and capability-passing — L4 where it earns it.
- **gRPC server reflection (F6):** runtime strict-descriptor export → fetch-and-convert-locally — L3.
- **Arrow Flight (F9):** the two-phase per-stream schema handshake — L3 handshake template (sans gRPC/protobuf, F10).
- **Smithy (F7/F8):** transport-agnostic IDL as authoring source of truth → custom SBE binding.
- **Avro/Confluent (F5):** the compact schema-**fingerprint/ID** — but in the *handshake* for skew detection, not per message.
- **BEEP (F11):** the reusable session/envelope-framing idea — while heeding its "lost to just-use-HTTP" fate via the MCP edge door.

---

## 6. Tiered same-version vs cross-version (resolving the open question)

This was the run's Open Q1. **Operator decision: resolved — there is no performance tier today.** The fleet is
model-latency-bound, so the same-version/cross-version split collapses into a single default with optional,
*measured* upgrades — negotiated as a body `codecId` over one unchanging envelope, never a forked substrate.

- **Default, every peer pair → JSON/CBOR over whatever L1 tier connects them.** Fast enough; the model turn is
  the bottleneck. Strictness comes from validate-on-ingest (§3), not the wire.
- **Cross-version / cross-language durability is *already* provided by the envelope, not by a binary body.**
  Numeric `templateId` + skip-by-stride at L2 (F3) + per-stream catalog at L3 give mixed-deployment and
  polyglot tolerance regardless of body codec. This is the key realization: the evolvability you wanted from
  SBE bodies is mostly an *envelope* property, and you keep it with JSON bodies.
- **Measured upgrade, per `templateId` → SBE (F3) or Cap'n Proto (F4)** only when a specific path is shown to
  be throughput- or zero-copy-bound — which by definition means a **non-LLM-gated** path (deterministic
  high-rate components; the "whole company off one binary" regime). Even there: measure first.
- **rkyv / `repr(C)` → deferred.** The most fragile pick (ABI/endianness-locked, same-version-only) for the
  least current benefit. Revisit only if a measured intra-Rust hot path both exists and is shown to need it;
  until then it is not worth a second codec's maintenance.

**The unifying trick still holds:** the **L2 envelope is identical across all of this** — only the L4
`codecId`, negotiated in the HELLO, varies, and peers negotiate the strongest codec they *share*. Today that's
JSON/CBOR everywhere; a binary codec rises only where both peers advertise it for a measured-hot `templateId`.
**Recommendation: ship the JSON seam end-to-end first** (envelope + handshake + validate-on-ingest + router
skip-by-stride); introduce a binary codec for an individual `templateId` only behind a measurement that
justifies it.

---

## 7. Confidence & risk ledger

| Decision | Status | Cite |
|---|---|---|
| Aeron for IPC + UDP unicast/multicast (L1 tiers 0–1) | **VERIFIED** | F1 |
| Off-LAN must be TCP/UDP-framed (ossification) | **VERIFIED** | F2 |
| Envelope shaped on SBE header; `blockLength`=stride; append-only skip | **VERIFIED** | F3 |
| Envelope `stride` = whole-body length (SOFH role), distinct from inner `blockLength` | **REASONED** (beyond F3, flagged) | extends F3 |
| Cap'n Proto for zero-copy + capability bodies (L4) | **VERIFIED** | F4 |
| gRPC-reflection-style runtime descriptor fetch (L3) | **VERIFIED** | F6 |
| Smithy as source-of-truth IDL; custom SBE binding (not rpcv2Cbor) | **VERIFIED** | F7, F8 |
| Two-phase per-stream handshake shape | **VERIFIED** | F9 |
| Avoid Flight whole-stack (HTTP/2+protobuf) | VERIFIED (medium, split vote) | F10 |
| Steal Avro fingerprint for handshake; reject per-message ref | **VERIFIED** | F5 |
| MCP-as-edge-adapter discipline; BEEP cautionary fate | VERIFIED (BEEP medium, single secondary) | F11 |
| ZeroMQ / nng / NATS / iceoryx as fallbacks | **REASONED-UNVERIFIED** | report caveat |
| QUIC standalone outside HTTP/3 | **REASONED-UNVERIFIED** | Open Q4 |
| **L4 default = JSON/CBOR**; binary codecs (SBE/Cap'n Proto) = measured per-type upgrades | **DECIDED** (operator — fleet is model-latency-bound) | — |
| Validate every body against the catalog on ingest (the JSON strictness gate) | **DECIDED** (standing rule) | forced by JSON default |
| rkyv / `repr(C)` intra-Rust fast path | **DEFERRED** (operator; revisit only on a measured need) | Open Q1 |
| Open DDS impls "too heavy, steal ideas" | **REASONED-UNVERIFIED** | report caveat |

**Top risks:**
1. **Envelope `stride` semantics** — if anyone wires the router to skip by raw SBE `blockLength` it will
   mis-advance past groups/var-data. The whole-body-length decision (§2 note) must be encoded and tested first.
2. **ZeroMQ-vs-Aeron for the fleet is unverified** (Open Q2) — ZeroMQ's missing reliability/persistence may or
   may not matter for an agent fleet; needs a measured bake-off, not a paper call.
3. **QUIC-standalone usability** (Open Q4) — don't commit the cross-boundary tier to QUIC until proven outside
   HTTP/3; TLS/TCP is the safe default meanwhile.
4. **Smithy→SBE custom-binding effort** (forced by F8) — non-trivial, but **lower urgency now**: with JSON the
   default body codec, the SBE binding is only needed for `templateId`s actually upgraded to SBE, so it can wait
   for the first measured hot path rather than gating v1.
5. **JSON strictness depends entirely on the ingest validator.** With a self-describing default codec, a missing
   or lax validate-on-ingest gate silently reintroduces the untyped looseness the substrate exists to remove.
   The validator is load-bearing — a validation gap is a correctness bug, not a nicety.

**De-risk first — minimal end-to-end slice:** three agents (two Rust + one non-Rust, e.g. Java or Python via
Smithy-generated stubs) exchanging **one `templateId`** over **Aeron IPC and Aeron UDP**, with: (a) the L2
envelope (`templateId`, whole-body `stride`, `schemaId`+`version`, `src/dst/correlationId`, CRC-32C); (b) a
per-stream HELLO negotiating the catalog + `codecId` (Flight two-phase shape); (c) one **JSON body, validated
against the catalog on ingest**; (d) a router that forwards on **envelope only** and is proven to **skip an
injected unknown-`templateId` frame by `stride`**; (e) an MCP edge adapter that round-trips that `templateId`
to JSON and back. Then add a v2 of the template with an extra field and prove the v1 receiver tolerates it with
**no flag day** (JSON: the validator ignores the unknown field; re-run on an SBE-upgraded `templateId` to prove
bounded-read by `blockLength`). That slice exercises F1, F3 (envelope shape), F6/F9, the **validate-on-ingest
gate**, and the §2 stride decision in one pass — if it holds, the composition holds.

---

## 8. Open decisions for the operator

1. **Owned-LAN default: Aeron vs ZeroMQ** (Open Q2). Aeron is verified-lean and matches the multicast
   background; ZeroMQ is your stated default but unverified here and lacks Aeron's reliability/persistence.
   Bake-off needed.
2. **Cross-boundary transport: QUIC-standalone vs TLS/TCP** (Open Q4). Defer to TLS/TCP until QUIC-outside-
   HTTP/3 is proven for your topology.
3. **Smithy as source of truth, or hand-authored SBE schemas?** Smithy buys polyglot stubs + a clean contract
   (F7) at the cost of writing and maintaining a custom SBE binding (F8). Worth it only if multiple languages
   consume the catalog.
4. **Catalog distribution model:** per-peer gRPC-reflection-style export (F6), a central Confluent-style
   registry (F5), or an embedded versioned descriptor shipped with each agent. Affects bootstrap and skew
   recovery.
5. **Integrity policy:** CRC-32C (corruption only), HMAC (authenticity), or none — per stream, via the
   envelope's optional-integrity field. Threat-model dependent; depends on whether Tier-2 links are trusted.
6. ~~Is the rkyv intra-Rust fast path worth a second codec?~~ **RESOLVED (operator):** deferred — the fleet is
   model-latency-bound, so JSON/CBOR is the L4 default and binary codecs (SBE/Cap'n Proto) are measured per-type
   upgrades. Revisit only if a non-LLM, high-rate path is measured to need it.
7. **Capability model:** adopt Cap'n Proto's capability RPC (F4) for inter-agent capability grants, or keep
   capabilities/authz out-of-band in the envelope routing layer? Pulls Cap'n Proto from "optional L4 codec"
   toward "structural dependency" if adopted.

---

*Sources are the verified URLs in `research/messaging-substrate-prior-art.json` / `research/messaging-substrate-prior-art.md` (F1–F11). No benchmarks or sources
beyond that set are invented; every pick beyond the verified findings is tagged REASONED-UNVERIFIED.*
