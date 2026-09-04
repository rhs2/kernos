/**
 * In-process stub of the kernel HTTP API for the client tests.
 *
 * Routes are registered per test as "METHOD /path" with a handler that gets
 * the recorded request and returns a status and body. Every request is kept
 * in `requests` so tests can assert on paths, query strings, headers and
 * bodies. Nothing here touches the network beyond the loopback listener.
 */
import { createServer, type IncomingHttpHeaders, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

export interface Recorded {
  method: string;
  path: string;
  query: URLSearchParams;
  headers: IncomingHttpHeaders;
  body: unknown;
  rawBody: string;
}

export interface Reply {
  status?: number;
  body?: unknown;
  raw?: string;
  headers?: Record<string, string>;
}

export type Handler = (req: Recorded) => Reply | Promise<Reply>;

export class Stub {
  readonly requests: Recorded[] = [];
  readonly routes = new Map<string, Handler>();
  private server: Server;
  url = "";

  private constructor() {
    this.server = createServer((req, res) => void this.handle(req, res));
  }

  static async start(): Promise<Stub> {
    const stub = new Stub();
    await new Promise<void>((resolve) => stub.server.listen(0, "127.0.0.1", resolve));
    const addr = stub.server.address() as AddressInfo;
    stub.url = `http://127.0.0.1:${addr.port}`;
    return stub;
  }

  /** Register a handler for an exact method and path (query string excluded). */
  on(method: string, path: string, handler: Handler | Reply): this {
    const h: Handler = typeof handler === "function" ? handler : () => handler;
    this.routes.set(`${method.toUpperCase()} ${path}`, h);
    return this;
  }

  /** The most recent recorded request. */
  last(): Recorded {
    const r = this.requests[this.requests.length - 1];
    if (!r) throw new Error("stub: no request recorded");
    return r;
  }

  reset(): void {
    this.requests.length = 0;
    this.routes.clear();
  }

  close(): Promise<void> {
    return new Promise((resolve) => this.server.close(() => resolve()));
  }

  private async handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    const rawBody = Buffer.concat(chunks).toString("utf8");
    let body: unknown = undefined;
    if (rawBody.length > 0) {
      try {
        body = JSON.parse(rawBody);
      } catch {
        body = rawBody;
      }
    }
    const url = new URL(req.url ?? "/", "http://stub");
    const recorded: Recorded = {
      method: (req.method ?? "GET").toUpperCase(),
      path: url.pathname,
      query: url.searchParams,
      headers: req.headers,
      body,
      rawBody,
    };
    this.requests.push(recorded);
    const handler = this.routes.get(`${recorded.method} ${recorded.path}`);
    let reply: Reply;
    if (!handler) {
      reply = { status: 404, body: { error: { code: "not_found", message: `no route for ${recorded.method} ${recorded.path}`, details: {} } } };
    } else {
      reply = await handler(recorded);
    }
    const status = reply.status ?? 200;
    const headers: Record<string, string> = { ...(reply.headers ?? {}) };
    let payload = "";
    if (reply.raw !== undefined) {
      payload = reply.raw;
      if (!headers["content-type"]) headers["content-type"] = "text/plain";
    } else if (reply.body !== undefined) {
      payload = JSON.stringify(reply.body);
      if (!headers["content-type"]) headers["content-type"] = "application/json";
    }
    res.writeHead(status, headers);
    res.end(payload);
  }
}
