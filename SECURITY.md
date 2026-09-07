# Security policy

CedeGrid controls trusted, explicitly submitted workloads. It is not a sandbox for
hostile programs. Root, the same operating-system account, trusted operator and node
credentials remain within the administrative trust boundary. See [guarantees](docs/guarantees.md).

For vulnerabilities, use GitHub private vulnerability reporting on this repository
when the Security tab offers Report a vulnerability. If that channel is unavailable,
open an issue asking for a private contact without including exploit details,
credentials, private logs or affected system identifiers. Do not post those details
until a private channel has been established. No response-time commitment is implied.

Report the affected revision and platform, the violated boundary, a minimal safe
reproducer and observed impact. Test only systems and processes you are authorized
to use. The initial 0.1 series has no long-term support branch; fixes target the
current development line. A security source review does not certify Linux kernel
controls, shared-server storage or every deployment configuration.
