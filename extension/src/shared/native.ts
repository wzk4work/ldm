// Host connection: Native Messaging first, optional localhost WebSocket bridge
// as a fallback.
import { envelope, isEnvelope, NATIVE_HOST_NAME, WS_SUBPROTOCOL, type Envelope } from "./protocol";

export interface Transport {
  readonly name: string;
  send(msg: unknown): void;
  close(): void;
  onMessage: ((msg: unknown) => void) | null;
  onDisconnect: ((reason?: string) => void) | null;
}

export class NativeTransport implements Transport {
  readonly name = "native";
  onMessage: ((msg: unknown) => void) | null = null;
  onDisconnect: ((reason?: string) => void) | null = null;
  private port: chrome.runtime.Port;
  constructor(host = NATIVE_HOST_NAME) {
    this.port = chrome.runtime.connectNative(host);
    this.port.onMessage.addListener((m: unknown) => this.onMessage?.(m));
    this.port.onDisconnect.addListener(() => this.onDisconnect?.(chrome.runtime.lastError?.message));
  }
  send(msg: unknown): void {
    this.port.postMessage(msg);
  }
  close(): void {
    this.port.disconnect();
  }
}

export class WsTransport implements Transport {
  readonly name = "ws";
  onMessage: ((msg: unknown) => void) | null = null;
  onDisconnect: ((reason?: string) => void) | null = null;
  private ws: WebSocket;
  private queue: string[] = [];
  constructor(port: number, token?: string) {
    const q = token ? `?token=${encodeURIComponent(token)}` : "";
    this.ws = new WebSocket(`ws://127.0.0.1:${port}/${q}`, [WS_SUBPROTOCOL]);
    this.ws.onopen = () => {
      for (const m of this.queue.splice(0)) this.ws.send(m);
    };
    this.ws.onmessage = (ev) => {
      try {
        this.onMessage?.(JSON.parse(String(ev.data)));
      } catch {
        /* ignore malformed */
      }
    };
    this.ws.onclose = (ev) => this.onDisconnect?.(`ws closed (${ev.code})`);
  }
  send(msg: unknown): void {
    const s = JSON.stringify(msg);
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(s);
    else this.queue.push(s);
  }
  close(): void {
    this.ws.close();
  }
}

type Pending = { resolve: (e: Envelope) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> };

export class HostClient {
  private transport: Transport | null = null;
  private pending = new Map<number, Pending>();
  private listeners = new Set<(e: Envelope) => void>();
  private statusListeners = new Set<(up: boolean, via?: string) => void>();
  private attempt = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  ready = false;
  private stopped = false;
  lastError: string | undefined;

  constructor(private factories: Array<() => Transport>, private hello: () => Envelope, private retryMs = 5000) {}

  onEvent(cb: (e: Envelope) => void): () => void {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  onStatus(cb: (up: boolean, via?: string) => void): void {
    this.statusListeners.add(cb);
  }

  private setReady(up: boolean): void {
    this.ready = up;
    for (const cb of this.statusListeners) cb(up, this.transport?.name);
  }

  connect(): void {
    this.stopped = false;
    if (this.transport) return;
    const f = this.factories[this.attempt % this.factories.length];
    let t: Transport;
    try {
      t = f();
    } catch (e) {
      this.lastError = String(e);
      this.scheduleRetry();
      return;
    }
    this.transport = t;
    t.onMessage = (m) => this.dispatch(m);
    t.onDisconnect = (reason) => {
      this.lastError = reason;
      this.transport = null;
      this.setReady(false);
      for (const [id, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error(`host disconnected: ${reason ?? "unknown"}`));
        this.pending.delete(id);
      }
      this.attempt++;
      this.scheduleRetry();
    };
    const h = this.hello();
    this.request(h.type, h.payload, 8000, h.id)
      .then((r) => {
        if (r.type === "HelloAck") {
          this.attempt = 0;
          this.setReady(true);
        }
      })
      .catch(() => undefined);
  }

  private scheduleRetry(): void {
    if (this.retryTimer || this.stopped) return;
    // try the next transport immediately once, then back off
    const delay = this.attempt % this.factories.length === 0 ? this.retryMs : 0;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, delay);
  }

  private dispatch(m: unknown): void {
    if (!isEnvelope(m)) return;
    const p = this.pending.get(m.id);
    if (p) {
      clearTimeout(p.timer);
      this.pending.delete(m.id);
      p.resolve(m);
    }
    for (const cb of this.listeners) cb(m);
  }

  request(type: string, payload: object, timeoutMs = 10000, id?: number): Promise<Envelope> {
    const env = envelope(type, payload, id);
    return new Promise((resolve, reject) => {
      if (!this.transport) {
        reject(new Error("host not connected"));
        return;
      }
      const timer = setTimeout(() => {
        this.pending.delete(env.id);
        reject(new Error(`timeout waiting for reply to ${type}`));
      }, timeoutMs);
      this.pending.set(env.id, { resolve, reject, timer });
      try {
        this.transport.send(env);
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(env.id);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  /** Fire-and-forget notification. */
  notify(type: string, payload: object): void {
    try {
      this.transport?.send(envelope(type, payload));
    } catch {
      /* host gone; ignored */
    }
  }

  disconnect(): void {
    this.stopped = true;
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    this.transport?.close();
    this.transport = null;
    this.setReady(false);
  }
}
