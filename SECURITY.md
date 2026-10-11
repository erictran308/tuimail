# Security

## Reporting a vulnerability

Report it privately at <https://github.com/erictran308/tuimail/security/advisories/new> (Security → Report a vulnerability), not in a public issue.

Say what a message, a server's answer or a file has to contain, what happens, your tuimail version (`tuimail --version`), OS and terminal.

## What counts

Anything a message you receive, a mail server, or a file tuimail is pointed at can do to you through tuimail. For example:

- a subject, name, text or file name that runs code, opens something without asking, or garbles the terminal;
- a message that makes tuimail fetch something from the internet, or tells its sender you read it;
- a server that gets tuimail to send your password without TLS, or to send commands it didn't build;
- an attachment saved over another file, or outside the folder it's saved in;
- your password, accounts or mail left where another account on the computer can read them.
