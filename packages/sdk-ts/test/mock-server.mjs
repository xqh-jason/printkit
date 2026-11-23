// Minimal RFC 6455 server for the protocol tests: no dependencies, one text
// frame per message, which is all the SDK client sends and expects.

import { createServer } from "node:http";
import { createHash } from "node:crypto";

const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

function decode(buffer) {
  if (buffer.length < 2) {
    return null;
  }
  const opcode = buffer[0] & 0x0f;
  const masked = (buffer[1] & 0x80) !== 0;
  let length = buffer[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buffer.length < 4) {
      return null;
    }
    length = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buffer.length < 10) {
      return null;
    }
    length = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  const maskLength = masked ? 4 : 0;
  if (buffer.length < offset + maskLength + length) {
    return null;
  }
  const mask = masked ? buffer.subarray(offset, offset + 4) : null;
  offset += maskLength;
  const payload = Buffer.from(buffer.subarray(offset, offset + length));
  if (mask !== null) {
    for (let index = 0; index < payload.length; index += 1) {
      payload[index] ^= mask[index % 4];
    }
  }
  return { opcode, payload, rest: buffer.subarray(offset + length) };
}

function encode(text) {
  const payload = Buffer.from(text, "utf8");
  let header;
  if (payload.length < 126) {
    header = Buffer.from([0x81, payload.length]);
  } else if (payload.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  return Buffer.concat([header, payload]);
}

function defaultResponder(capabilities) {
  return (method) => {
    switch (method) {
      case "sys.info":
        return {
          result: { engine: "0.1.0", protocol: 1, platform: "macos", capabilities },
        };
      case "printers.list":
        return {
          result: [
            {
              name: "Mock Printer",
              isDefault: true,
              raw: true,
              mimeTypes: ["application/pdf", "application/vnd.cups-raw"],
              duplex: true,
              paperSource: true,
            },
          ],
        };
      case "printers.papers":
        return {
          result: [{ name: "A4", widthMm: 210, heightMm: 297, kind: "named" }],
        };
      case "doc.print":
      case "doc.printPdf":
      case "raw.send":
        return { result: { jobId: "job-1" } };
      case "doc.printBatch":
        return { result: { jobId: "job-1", docCount: 2, pageCount: 2 } };
      case "doc.printPdfBatch":
        return { result: { jobId: "job-1", pages: 2 } };
      case "doc.preview":
        return { result: { windowId: "win-1" } };
      case "preview.pages":
        return { result: { png: "iVBORw0KGgo=" } };
      case "preview.close":
      case "job.cancel":
        return { result: {} };
      case "doc.exportPdf":
        return { result: { path: "exports/quotes/2026-11.pdf", pages: 1, bytes: 42 } };
      case "job.status":
        return {
          result: { jobId: "job-1", status: "printing", queuePos: 1, pages: 1 },
        };
      case "records.list":
        return {
          result: {
            records: [
              {
                id: 1,
                seq: 1,
                at: "2026-01-01T00:00:00Z",
                origin: "http://127.0.0.1:5173",
                tags: ["quotes"],
                pages: 1,
                copies: 1,
                status: "done",
                durMs: 12,
                traceId: "trace-1",
              },
            ],
            nextCursor: 1,
          },
        };
      case "records.stats":
        return {
          result: { buckets: [{ key: "quotes", count: 1, sheets: 1, avgDurMs: 12 }] },
        };
      case "records.summary":
        return {
          result: {
            todaySheets: 1,
            todayDoneSheets: 1,
            todayDone: 1,
            todayFailed: 0,
            todayCancelled: 0,
            total: 1,
            failed: 0,
            cancelled: 0,
            pending: 0,
            avgDurMs: 12,
            slowestAvgDurMs: 0,
          },
        };
      case "records.export":
        return { result: { csv: "\ufeffid,at\n1,2026-01-01T00:00:00Z\n" } };
      default:
        return { error: { code: "E_PROTOCOL", message: `unexpected method ${method}` } };
    }
  };
}

export async function createMockService(options = {}) {
  const capabilities = {
    duplex: true,
    paperSource: true,
    cancelSubmitted: true,
    cancelRaw: true,
    preview: true,
    rasterFallback: true,
    raw: true,
    batch: true,
    pdfPassthrough: true,
    renderWorkers: 1,
    ...options.capabilities,
  };
  const welcomeCapabilities = options.welcomeCapabilities ?? [];
  const baseResponder = defaultResponder(capabilities);
  // A test responder may override one method and leave the rest to the default.
  const respond = (method, params, message) =>
    options.respond?.(method, params, message) ?? baseResponder(method, params, message);
  const requests = [];
  const handshakes = [];
  const sockets = new Set();

  const server = createServer((_, response) => {
    response.statusCode = 404;
    response.end();
  });

  server.on("upgrade", (request, socket) => {
    const key = request.headers["sec-websocket-key"] ?? "";
    const accept = createHash("sha1")
      .update(key + GUID)
      .digest("base64");
    socket.write(
      [
        "HTTP/1.1 101 Switching Protocols",
        "Upgrade: websocket",
        "Connection: Upgrade",
        `Sec-WebSocket-Accept: ${accept}`,
        "",
        "",
      ].join("\r\n"),
    );
    sockets.add(socket);
    let buffered = Buffer.alloc(0);
    // Optional server-side idle close, so a test can prove the SDK heartbeat
    // resets the service's inactivity window instead of trusting a timer.
    let idleTimer = null;
    const touch = () => {
      if (options.idleCloseMs) {
        clearTimeout(idleTimer);
        idleTimer = setTimeout(() => socket.end(), options.idleCloseMs);
      }
    };
    touch();
    socket.on("close", () => clearTimeout(idleTimer));

    const send = (frame) => socket.write(encode(JSON.stringify(frame)));

    socket.on("data", (chunk) => {
      touch();
      buffered = Buffer.concat([buffered, chunk]);
      for (;;) {
        const frame = decode(buffered);
        if (frame === null) {
          return;
        }
        buffered = frame.rest;
        if (frame.opcode === 0x8) {
          socket.end();
          return;
        }
        if (frame.opcode !== 0x1) {
          continue;
        }
        const message = JSON.parse(frame.payload.toString("utf8"));
        if (message.type === "hello") {
          handshakes.push(message);
          if (message.token === "rejected-token") {
            send({
              type: "rejected",
              error: { code: "E_AUTH", message: "unknown token", detail: { token: "masked" } },
            });
            socket.end();
            continue;
          }
          send({
            type: "welcome",
            protocol: 1,
            engine: "0.1.0",
            granted: ["print", "raw"],
            capabilities: welcomeCapabilities,
            warnings: [],
          });
          continue;
        }
        requests.push(message);
        const outcome = respond(message.method, message.params, message);
        const reply = { id: message.id, ok: outcome.error === undefined };
        if (outcome.error === undefined) {
          reply.result = outcome.result ?? {};
        } else {
          reply.error = outcome.error;
        }
        const delay = options.delays?.[message.method];
        if (delay === undefined) {
          send(reply);
        } else {
          setTimeout(() => {
            if (socket.writable) {
              send(reply);
            }
          }, delay);
        }
      }
    });
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => sockets.delete(socket));
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();

  return {
    url: `ws://127.0.0.1:${port}/print`,
    requests,
    handshakes,
    pushEvent(kind, payload) {
      for (const socket of sockets) {
        socket.write(encode(JSON.stringify({ type: "event", event: kind, ...payload })));
      }
    },
    dropSockets() {
      for (const socket of sockets) {
        socket.destroy();
      }
      sockets.clear();
    },
    async close() {
      for (const socket of sockets) {
        socket.destroy();
      }
      sockets.clear();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}
