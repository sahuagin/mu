# Recovered research report — inter-agent messaging substrate prior art (mu)

> Reconstructed post-hoc from 96 on-disk agent transcripts. The deep-research workflow stalled at the
> Synthesize stage; this document re-runs that stage's logic by hand. All sources are real URLs taken
> from the verified transcript data.

**Research question (recovered, verbatim from 95 downstream prompts):**
Expert-level (HFT / exchange / market-data architect audience) prior-art survey to *leverage, extend, or
steal ideas from* for a strictly-typed, event-driven inter-agent messaging substrate for the "mu"
multi-agent fleet — with MCP (JSON-RPC over stdio/HTTP+SSE) demoted from "the substrate" to a thin edge
adapter. The architecture is **settled**: L1 transport / L2 self-describing routable envelope that the
router reads *exclusively* (msg-type, stride=length-to-skip, schema-version, routing, optional integrity)
/ L3 numeric template-id schema in an owned namespace, established once **per-stream** at handshake / L4
pluggable per-type body codec opaque to the router; transport separate (brokered or peer-to-peer,
language-agnostic at the cross-language seam, local UDP acceptable, mixed versions skip-by-stride). Survey
three pillars — **Transport**, **Serialization/format**, **Typed discovery/self-description** — plus
session-framing prior art (BEEP), and find whether an existing stack already composes these layers
(xkcd-927 avoidance).

---

## Executive summary

For mu's settled L1–L4 design, the verified prior art splits cleanly by pillar and confirms the operator's
instincts rather than overturning them. On **serialization**, SBE is a direct match — its `blockLength`
*is* the envelope's stride/length-to-skip, and its append-only, bounded-read evolution is exactly the
mixed-version "old receivers skip unknown" model — while Cap'n Proto adds zero-copy plus bundled capability
RPC, and Avro/Confluent show that the "schema repeated per message" objection is solved out-of-band by
fingerprint/ID reference (though that remains schema-*reference*-per-message, not schema-per-stream-at-
handshake). On **typed discovery**, gRPC server reflection and AWS **Smithy** both deliver the "caller
fetches the strict type and converts locally" shape; Smithy is the strongest *protocol/transport-agnostic*
contract fit, but its blessed binary protocol (rpcv2Cbor) is nailed to HTTP POST and is name-self-
describing, so running a Smithy contract over Aeron/ZeroMQ means authoring your own binding. On
**transport**, the lean HFT-native pick is Aeron (reliable UDP unicast+multicast+IPC, language-agnostic
clients), and the decisive constraint is middlebox ossification — only TCP/UDP survive networks you don't
own, which is why SCTP stays on owned LANs and QUIC rides UDP. **No single existing stack composes all
three pillars the way mu wants without dragging in HTTP/2 + protobuf** (gRPC, Arrow Flight) — so the
evidence points to *composing* (SBE/Cap'n Proto typed core + Aeron transport + a reflection/Smithy-style
typed-discovery layer), stealing BEEP's reusable-session-framing idea while heeding BEEP's own cautionary
fate of losing to "just use HTTP."

## Caveats

- **This is a reconstruction, not the workflow's own output.** The run stalled at Synthesize; one verifier
  vote also hung (the Confluent Schema Registry claim carries 2 votes, not 3 — it still passed 2-0).
- **The scope stage degenerated.** The decomposition agent emitted placeholder angles/question
  (`question:"test"`, angles labelled a–e with queries b–f) after repeated schema failures; the workflow
  nevertheless drove all three searchers with the *real* full question re-injected per prompt. Net effect:
  the intended 5-angle spread collapsed, so **transport-pillar breadth is thin** — ZeroMQ (the operator's
  own default), nng, iceoryx, NATS/JetStream and the open DDS impls were fetched into sources but their
  claims did not survive into the verified top-25. The verified set skews toward serialization + typed
  discovery.
- **The adversarial layer killed nothing (0 of 25).** This is a low-controversy, mostly definitional claim
  set checked against primary docs, so the skeptic filter did little real work — treat unanimous "high"
  verdicts as "faithfully quotes a primary source," not as "contested and survived."
- **Source quality / dating.** Several primary sources are undated GitHub READMEs / living spec pages; the
  BEEP source is a single paywalled 2002 book preview (secondary); the middlebox-ossification finding rests
  on a single secondary explainer. Cap'n Proto's "∞× faster than protobuf" is marketing framing — the
  verifier confirmed the zero-copy *mechanism*, not a literal speedup.

## Open questions

1. **The recommended composition was never produced** — that is precisely the stage that hung. Which single
   tiered stack (e.g. rkyv/`repr(C)` for same-version intra-fleet Rust hot paths + SBE/Cap'n Proto/CBOR for
   the cross-version/cross-language seam + a typed-discovery mechanism + a CloudEvents-style envelope)?
2. **ZeroMQ vs Aeron for mu specifically.** The lean default (ZeroMQ) has no surviving verified claim; how
   do its missing reliability/persistence layers actually weigh against Aeron for a fleet of agents?
3. **Where exactly does MCP survive as the edge adapter** — no verified claim addresses the LCD-edge ↔
   typed-core seam in concrete terms.
4. **Transport when it leaves home.** Only the ossification half is verified; QUIC's usability *standalone*
   (outside HTTP/3) and the SCTP-on-trusted-LAN-vs-QUIC split remain unconfirmed.

---

## Confirmed findings

### F1 — Aeron is the lean, language-agnostic HFT-native transport (reliable UDP unicast + multicast + IPC)
- **Confidence:** high (primary source, unanimous 3-0 ×2 claims)
- **Sources:** https://github.com/real-logic/aeron
- **Evidence:** Aeron provides reliable UDP unicast, UDP multicast, *and* same-machine IPC (shared memory)
  in one library — covering exactly the transport options the architecture flags (local-UDP acceptable;
  multicast as in the operator's prior RTI DDS market-data work), positioning it as the lean alternative to
  a full DDS. It is language-agnostic at the client seam with first-class Java, C, C++11 and .NET clients
  (canonical/highest-fidelity client is JVM, native path is C/C++), satisfying the cross-language-seam
  requirement.

### F2 — Transport choice is bounded by middlebox ossification: only TCP/UDP survive networks you don't own
- **Confidence:** medium (single secondary explainer, unanimous 3-0)
- **Sources:** https://http3-explained.haxx.se/en/why-quic/why-tcpudp
- **Evidence:** Firewalls, NATs and routers reliably pass only TCP or UDP, so any non-TCP/UDP transport
  cannot be deployed across uncontrolled networks. This is the direct mechanism that confined SCTP to
  controlled LANs and forced QUIC onto UDP — i.e. mu's "local-network UDP acceptable" is fine on owned
  boxes, but anything leaving them must be TCP/UDP-framed.

### F3 — SBE confirms the operator's envelope model: blockLength = stride, append-only, bounded-read evolution
- **Confidence:** high (primary source, unanimous 3-0 ×3 claims)
- **Sources:** https://github.com/real-logic/simple-binary-encoding/wiki/Message-Versioning
- **Evidence:** SBE encodes the root block's total fixed size as `blockLength` in the message header —
  directly confirming the independently re-derived mapping `blockLength = stride / length-to-skip`, the
  basis for old-receiver forward compatibility. Evolution is strictly append-only (fields added only at the
  end of a block; existing fields never modified/removed), and forward/backward compatibility is enforced
  by bounding reads to the encoded block length: a newer decoder reading older data stops at the old block
  and null-fills absent fields, while an old decoder skips appended fields via the larger encoded
  `blockLength`. This is the schema-per-stream, skip-by-length model the whole mu architecture is built on.

### F4 — Cap'n Proto is a genuine multi-pillar candidate: zero-copy format + bundled capability RPC
- **Confidence:** high (primary source, unanimous 3-0 ×2 claims)
- **Sources:** https://github.com/capnproto/capnproto
- **Evidence:** Cap'n Proto's wire layout *is* its in-memory layout, so there is effectively no
  encode/decode parse step (the "∞× faster than Protocol Buffers" framing) — direct support for the
  operator's Pillar-2 zero-copy interest and the Cap'n-Proto-supersedes-protobuf hypothesis. It also bundles
  a capability-based RPC system with the serialization format in one project, making it span format + RPC
  rather than a serialization library alone.

### F5 — Avro + Confluent resolve "schema repeated per message" out-of-band (fingerprint / schema-ID), but it's reference-per-message not per-stream-handshake
- **Confidence:** high (primary sources, unanimous among cast votes; note the Confluent claim rode 2 votes — one verifier hung)
- **Sources:** https://avro.apache.org/docs/1.11.1/specification/ · https://docs.confluent.io/platform/current/schema-registry/index.html
- **Evidence:** Avro "single-object encoding" does **not** embed the full schema per message: the wire is a
  2-byte magic marker (C3 01) + an 8-byte little-endian CRC-64-AVRO schema fingerprint + the binary object —
  ~10 bytes overhead, schema resolved out of band. Confluent Schema Registry does the equivalent with a
  compact schema **ID** on the wire, schema registered once centrally. Compatibility is resolved at decode
  time by separate writer/reader schemas matched **by field name** (writer-only fields skipped;
  reader-only fields default-filled). This squarely answers the operator's Avro objection — but it is a
  schema-*reference*-per-message model, explicitly **not** SBE's schema-established-once-per-stream-at-
  handshake.

### F6 — gRPC server reflection is a runtime typed-discovery layer: the server exports its strict protobuf descriptors; the client fetches the type and converts locally
- **Confidence:** high (primary sources, unanimous 3-0 ×4 claims)
- **Sources:** https://github.com/grpc/grpc/blob/master/doc/server-reflection.md · https://grpc.io/docs/guides/reflection/
- **Evidence:** Server reflection is an optional server-side extension that lets clients build requests at
  **runtime** with no precompiled stub/schema — the server declares its protobuf-defined APIs (including all
  referenced types) over a standardized RPC service, and the discovery payload is the strict machine-readable
  descriptor itself, not a stringly/human-only description. The client uses that schema to encode requests
  and decode responses at runtime — a near-exact match for mu's "caller fetches the up-to-date strict type
  and converts locally" (Pillar 3).

### F7 — Smithy is the strongest "strict contract held separate from transport" fit: a protocol/transport/language-agnostic IDL
- **Confidence:** high (primary sources, unanimous 3-0 ×3 claims)
- **Sources:** https://github.com/smithy-lang/smithy · https://smithy.io/2.0/
- **Evidence:** Smithy is a protocol-agnostic interface definition language whose tooling generates clients,
  servers and docs for any language, explicitly designed to be language-, environment-, transport- and
  serialization-agnostic. It decouples the transport layer from a service's data structures and capabilities
  so the two evolve independently — exactly the "strict contract, caller converts, transport-held-separate"
  shape mu wants in Pillar 3.

### F8 — Caveat to F7: Smithy's blessed binary protocol (rpcv2Cbor) is nailed to HTTP POST and is name-self-describing — not a numeric-template-id skip-by-length stream
- **Confidence:** high (primary source, unanimous 3-0 ×2 claims)
- **Sources:** https://smithy.io/2.0/additional-specs/protocols/smithy-rpc-v2.html
- **Evidence:** smithy-rpc-v2 (rpcv2Cbor) is a concrete HTTP binding: every request is HTTP POST, all
  Smithy HTTP-binding traits are ignored, and the body carries everything as a CBOR document. Its CBOR
  encodes structs/unions as major-type-5 maps keyed **by member name** (field names ride on the wire as
  text keys), not by numeric tag/template-id — so there is no `stride`/`blockLength` skip-by-length frame.
  Smithy *the IDL* is transport-agnostic, but its AWS-blessed wire protocol is not: to run a Smithy contract
  over ZeroMQ/Aeron/raw-UDP you must author your own protocol binding — rpcv2Cbor is not it.

### F9 — Arrow Flight is a whole-stack Pillar-3 candidate: schema-per-stream (two-phase) + built-in typed discovery RPCs
- **Confidence:** high (primary source, unanimous 3-0 ×2 claims)
- **Sources:** https://arrow.apache.org/docs/format/Flight.html
- **Evidence:** Flight is schema-per-stream: the client first calls `GetFlightInfo(FlightDescriptor)` to get
  a `FlightInfo` carrying schema + endpoints, then separately `DoGet(Ticket)` to pull the record-batch
  stream — schema fetched once up front, not per message. It also ships built-in typed discovery RPCs that
  map onto mu's "what typed messages/commands do you support?": `GetFlightInfo` (how a flight is consumed),
  `ListFlights` (enumerate streams) and `ListActions` (enumerate action types with human-readable
  descriptions).

### F10 — Arrow Flight rides gRPC + protobuf for all RPC/wire formats, inheriting HTTP/2 and the operator's protobuf objections
- **Confidence:** medium (split vote 2-1 — one verifier refuted; primary source)
- **Sources:** https://arrow.apache.org/docs/format/Flight.html
- **Evidence:** Flight is an RPC framework built on top of gRPC and the Arrow IPC format, with all RPC
  methods and wire message formats defined by Protocol Buffers (not a bespoke binary header). That makes it
  a gRPC/protobuf whole-stack candidate — it inherits gRPC's HTTP/2 transport and the operator's stated
  protobuf objections. (This is the only verified claim that drew a refuting vote; treat the protobuf-
  inheritance framing as medium-confidence.)

### F11 — BEEP is direct prior art for a reusable L2 envelope + session layer — and its fate is the cautionary tale
- **Confidence:** medium (single secondary source — paywalled 2002 book preview — unanimous 3-0 ×2 claims)
- **Sources:** https://www.oreilly.com/library/view/beep-the-definitive/9780596156954/ch01.html
- **Evidence:** BEEP (Marshall Rose) is a reusable application-protocol framework that factors out the
  machinery common to most application protocols, so a designer implements only domain-specific details —
  direct prior art for a reusable L2 envelope + session layer beneath mu's domain message types. The same
  source frames HTTP's gravitational pull — familiarity, ubiquity, simple request/response, firewall
  traversal — as why designers default to HTTP even when it's the wrong fit. That is the exact "just use
  HTTP" force that displaced BEEP: a cautionary tale that a correct-but-obscure stack others must implement
  loses to the familiar, ubiquitous, firewall-friendly option integrators already have.

---

## Refuted claims (for transparency)

None. All 25 claims that reached 3-vote adversarial verification survived (0 killed). One claim — the
Confluent Schema Registry finding (folded into F5) — was adjudicated on only 2 votes because the third
verifier hung (`agent-af279348d3876c8f3`); it passed 2-0. The single split vote anywhere in the run is the
Arrow-Flight-on-gRPC/protobuf claim (F10), which drew one refutation but survived 2-1.

---

## Sources

All 17 sources fetched (url · quality · angle):

- https://github.com/real-logic/aeron · primary · e
- https://github.com/real-logic/simple-binary-encoding/wiki/Message-Versioning · primary · a
- https://github.com/capnproto/capnproto · primary · c
- https://capnproto.org/news/2014-06-17-capnproto-flatbuffers-sbe.html · blog · a
- https://en.wikipedia.org/wiki/Cap'n_Proto · secondary · e
- https://avro.apache.org/docs/1.11.1/specification/ · primary · c
- https://docs.confluent.io/platform/current/schema-registry/index.html · primary · e
- https://github.com/confluentinc/schema-registry/issues/1294 · forum · c
- https://github.com/grpc/grpc/blob/master/doc/server-reflection.md · primary · e
- https://grpc.io/docs/guides/reflection/ · primary · c
- https://github.com/smithy-lang/smithy · primary · e
- https://smithy.io/2.0/ · primary · c
- https://smithy.io/2.0/additional-specs/protocols/smithy-rpc-v2.html · primary · e
- https://arrow.apache.org/docs/format/Flight.html · primary · e
- https://sanj.dev/post/aeron-alternatives-messaging-comparison/ · blog · a
- https://http3-explained.haxx.se/en/why-quic/why-tcpudp · secondary · a
- https://www.oreilly.com/library/view/beep-the-definitive/9780596156954/ch01.html · secondary · a

---

## Recovery stats

Reconstructed from 96 agent transcripts (1 scope · 3 search · 17 fetch · 75 verify) · 3 angles (degenerate
labels a/c/e) · 17 sources fetched · 79 claims extracted · top 25 verified by 3-vote adversarial review ·
**25 confirmed · 0 killed** · 1 hung verifier vote (Confluent claim, 2-0). The workflow stalled at the
Synthesize stage; the findings above are a faithful post-hoc reconstruction of that stage's logic, citing
only URLs present in the verified transcript data.
