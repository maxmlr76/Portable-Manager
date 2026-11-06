# Security policy

Please do **not** report security problems in public issues.

Use GitHub's private reporting instead: on the repository page open **Security**, then
**Report a vulnerability**. Describe the problem and how to reproduce it.

This is a spare-time project, so there is no guaranteed response time. Fixes go into the
latest release only.

## What is in scope

- The app or its engine installing something other than the verified download
  (checksum check, unpacking outside the tool folder, path handling)
- Code running from a manifest or a tool without the user starting it

## What is out of scope

Vulnerabilities in the tools the app can download. Please report those to their publishers.
