# Public test-only TLS credentials

These generated certificates and the deliberately public receiver private key
are used only by the local control-channel regression test. They are not trust
anchors for real Cast devices, are never installed, and must never protect real
traffic. The root private key is not retained. This fixture tests our current
peer-chain policy, not Cast DeviceAuth identity validation.

`test-device-{a,b}.der` and their equally public keys stand in for two Cast
device certificates. The local receiver signs DeviceAuth answers with them so
the remembered-identity check runs through the real connection code. They are
self-signed and prove nothing about any real device.

The certificate expiry is intentionally long to avoid annual fixture churn.
It uses the real certificate verifier and requires a clock within the fixture
validity period (2026–2126); no production verification bypass is added.
