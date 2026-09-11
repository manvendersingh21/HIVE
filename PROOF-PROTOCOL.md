# WebSocket File Transfer Protocol

## Overview
Simple authenticated bidirectional file transfer over WebSocket with SHA-256 verification.

## Transport
- Connection: WebSocket over secure SSH tunnel (port forwarding)
- Message format: JSON lines (one message per line, newline-terminated)
- Max message size: 8 KB (including JSON overhead)
- Max file size: 1 TB
- Keepalive: Either endpoint sends `{"type":"ping"}` if 10s silence; respond with `{"type":"pong"}`

## Authentication
1. Server starts listening on localhost:8765
2. Client reads `~/.hive/proof/token.txt` for auth token
3. Client sends: `{"type":"auth","token":"<token>"}`
4. Server responds: `{"type":"auth_ok"}` or `{"type":"auth_failed"}`
5. Connection closes on auth failure

## Send File
**Initiator sends:** `{"type":"send","path":"quoted.txt","size":1073741824}`
**Receiver responds:** `{"type":"ready"}` or `{"type":"error","reason":"..."}`

Then initiator streams chunks:
- `{"type":"chunk","offset":0,"data":"<base64>","size":8000}` (size is original byte count)
- `{"type":"chunk","offset":8000,"data":"<base64>","size":8000}`
- ...continue until offset+size == file size
- `{"type":"done","hash":"sha256:abc123..."}`

Receiver streams acks after each chunk:
- `{"type":"ack","offset":8000}` (confirming receipt up to this offset)

Receiver sends final validation:
- `{"type":"ok","path":"quoted.txt","hash":"sha256:abc123..."}` (hashes match)
- `{"type":"error","reason":"hash mismatch"}` (abort)

## Receiver-Initiated Get
**Receiver sends:** `{"type":"get","path":"requested.txt"}`
**Initiator responds:** `{"type":"file_info","size":1073741824}` or `{"type":"error","reason":"..."}`

Then initiator sends file (same chunk protocol as above).

## Error Handling
- Invalid path (contains `..`, starts with `/`, etc): respond `{"type":"error","reason":"invalid path"}`
- Missing file: `{"type":"error","reason":"not found"}`
- Authentication failed: close connection
- Interrupted transfer: client can reconnect and resume by asking for same file again (start fresh, no resumption)
- Timeout: Either end closes if 30s without message

## File Paths
- All paths relative to `~/hive-workspaces/proof-<date>/`
- Requests with quoted filenames like `send 'file name.txt'` should work
- Shell escaping is caller's responsibility
- Stored with exact requested name
