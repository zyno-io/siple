Signed test fixtures for the update installer tests. `test-key.sec` is a throwaway passwordless
minisign key made only for these tests (public half: `test-key.pub`); it is not, and must never
be, a release key. `SHA256SUMS.<version>` list the SHA-256 of the string `<version>/<file name>`
(the tests serve that string as the file's content), signed with the trusted comment
`siple v<version> SHA256SUMS`. The `.replay.minisig` is the 0.2.0 list signed with the comment of
another release.
