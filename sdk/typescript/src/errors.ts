/**
 * Errors thrown by the client.
 *
 * `KernosError` is any non-2xx response, carrying the kernel's stable error
 * code. `KernosNetworkError` is a transport failure: the request never got an
 * HTTP response (connection refused, DNS, abort, a body that could not be read).
 */

/** A non-2xx response from the kernel. */
export class KernosError extends Error {
  /** HTTP status of the response. */
  readonly status: number;
  /** Stable snake_case code from the error envelope, or `http_<status>` when the body had none. */
  readonly code: string;
  /** The `details` object of the error envelope, empty when absent. */
  readonly details: Record<string, unknown>;
  /** HTTP method and URL of the failed request. */
  readonly request: { method: string; url: string };

  constructor(
    status: number,
    code: string,
    message: string,
    details: Record<string, unknown> = {},
    request: { method: string; url: string } = { method: "", url: "" },
  ) {
    super(message);
    this.name = "KernosError";
    this.status = status;
    this.code = code;
    this.details = details;
    this.request = request;
  }

  /** Build an error from a response status and its raw body text. */
  static fromResponse(status: number, bodyText: string, request: { method: string; url: string }): KernosError {
    let parsed: unknown = undefined;
    if (bodyText.length > 0) {
      try {
        parsed = JSON.parse(bodyText);
      } catch {
        parsed = undefined;
      }
    }
    if (parsed && typeof parsed === "object" && "error" in parsed) {
      const err = (parsed as { error: unknown }).error;
      if (err && typeof err === "object") {
        const e = err as { code?: unknown; message?: unknown; details?: unknown };
        const code = typeof e.code === "string" && e.code.length > 0 ? e.code : `http_${status}`;
        const message = typeof e.message === "string" ? e.message : `${request.method} ${request.url} returned ${status}`;
        const details =
          e.details && typeof e.details === "object" && !Array.isArray(e.details)
            ? (e.details as Record<string, unknown>)
            : {};
        return new KernosError(status, code, message, details, request);
      }
    }
    const snippet = bodyText.length > 200 ? bodyText.slice(0, 200) + "..." : bodyText;
    const message =
      snippet.length > 0
        ? `${request.method} ${request.url} returned ${status}: ${snippet}`
        : `${request.method} ${request.url} returned ${status}`;
    return new KernosError(status, `http_${status}`, message, {}, request);
  }
}

/** The request did not produce an HTTP response. */
export class KernosNetworkError extends Error {
  /** The underlying error thrown by `fetch` or the body reader. */
  override readonly cause: unknown;
  /** HTTP method and URL of the failed request. */
  readonly request: { method: string; url: string };

  constructor(message: string, cause: unknown, request: { method: string; url: string } = { method: "", url: "" }) {
    super(message);
    this.name = "KernosNetworkError";
    this.cause = cause;
    this.request = request;
  }
}
