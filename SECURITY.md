# Security policy

Report vulnerabilities privately through the repository's Security tab.

## Release signing

OxidePM 0.4.0-rc.4 is the first release with a Minisign signature for `checksums.txt`.
The signature authenticates the checksum file and binds it to the exact release tag.
The checksum file covers the release archives, standalone adapter archives, and Debian packages.
Follow the [download verification instructions](README.md#verify-a-release-download)
before installing a release manually.

| Role | Key ID | Minisign public key |
| --- | --- | --- |
| Current | `04BF7D2A1951790C` | `RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC` |
| Next, kept offline | `63BC416033EA24EE` | `RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5` |

## Key rotation

We ship both public keys while signing with the current key. Once clients have both
pins, we can switch to the next signer. A later release ships the new signing key's
public key and a new spare.

Removing a compromised pin requires an updated client. We announce emergency rotations
through a trusted project channel so users can verify and install the replacement manually.
