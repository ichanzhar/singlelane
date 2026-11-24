# Test fixtures

`test-app-key.pkcs1.b64` and `test-app-key.pkcs8.b64` are the base64 bodies of one RSA key
generated only for these tests (the same key in PKCS#1 and PKCS#8 form). It is not, and never
was, the key of any GitHub App. The PEM marker lines are added at runtime by the tests, so
secret scanners don't flag the files.

`api/*.json` are trimmed, recorded-style GitHub REST responses. Tokens in them are
placeholders (`{{TOKEN}}`) replaced at runtime, and `{{BASE}}` is the mock server URL.
