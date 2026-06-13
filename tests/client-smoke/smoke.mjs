// node-redis smoke vs infinityd (M1-S03/S14). RESP3 by default in v4 when
// the server supports HELLO 3 — exercises protocol negotiation plus the same
// surface as the python smoke.
//
// Usage: node smoke.mjs [host [port]]
import { createClient } from "redis";

const host = process.argv[2] ?? "127.0.0.1";
const port = Number(process.argv[3] ?? 6379);

function assert(cond, what) {
  if (!cond) throw new Error(`client-smoke: ${what}`);
}

const client = createClient({ socket: { host, port } });
client.on("error", (e) => {
  console.error("client error:", e);
  process.exit(1);
});
await client.connect();

assert((await client.ping()) === "PONG", "PING");
assert((await client.set("smoke:js", "v")) === "OK", "SET");
assert((await client.get("smoke:js")) === "v", "GET");
assert((await client.incr("smoke:js:ctr")) >= 1, "INCR");
assert((await client.expire("smoke:js", 100)) === true, "EXPIRE");
const ttl = await client.ttl("smoke:js");
assert(ttl > 0 && ttl <= 100, `TTL ${ttl}`);

const info = await client.info();
for (const field of ["redis_version", "connected_clients", "used_memory"]) {
  assert(info.includes(field), `INFO missing ${field}`);
}

await client.clientSetName("smoke-js");
assert((await client.clientGetName()) === "smoke-js", "CLIENT GETNAME");

for (let i = 0; i < 50; i++) await client.set(`smoke:js:scan:${i}`, "x");
const seen = new Set();
for await (const key of client.scanIterator({ MATCH: "smoke:js:scan:*", COUNT: 13 })) {
  seen.add(key);
}
assert(seen.size === 50, `SCAN saw ${seen.size}/50`);

// Pub/sub: a dedicated subscriber connection (node-redis design).
const sub = client.duplicate();
await sub.connect();
const got = new Promise((resolve) => sub.subscribe("smoke:js:chan", resolve));
const receivers = await client.publish("smoke:js:chan", "hello");
assert(receivers === 1, `PUBLISH receivers ${receivers}`);
assert((await got) === "hello", "pub/sub delivery");
await sub.unsubscribe("smoke:js:chan");
await sub.quit();
await client.quit();

console.log("node-redis smoke: OK");
