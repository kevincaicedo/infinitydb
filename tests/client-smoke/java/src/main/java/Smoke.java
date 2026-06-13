// lettuce smoke vs infinityd (M1-S03/S14): connection negotiation, strings,
// expiry, INFO, SCAN cursor loop, pub/sub round trip.

import io.lettuce.core.RedisClient;
import io.lettuce.core.ScanArgs;
import io.lettuce.core.ScanCursor;
import io.lettuce.core.KeyScanCursor;
import io.lettuce.core.api.StatefulRedisConnection;
import io.lettuce.core.api.sync.RedisCommands;
import io.lettuce.core.pubsub.StatefulRedisPubSubConnection;
import io.lettuce.core.pubsub.RedisPubSubAdapter;

import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public final class Smoke {
    static void need(boolean cond, String what) {
        if (!cond) {
            System.err.println("client-smoke FAILED: " + what);
            System.exit(1);
        }
    }

    public static void main(String[] args) throws Exception {
        String host = args.length > 0 ? args[0] : "127.0.0.1";
        int port = args.length > 1 ? Integer.parseInt(args[1]) : 6379;
        RedisClient client = RedisClient.create("redis://" + host + ":" + port);

        try (StatefulRedisConnection<String, String> conn = client.connect()) {
            RedisCommands<String, String> r = conn.sync();
            need("PONG".equals(r.ping()), "PING");
            need("OK".equals(r.set("smoke:java", "v")), "SET");
            need("v".equals(r.get("smoke:java")), "GET");
            need(r.incr("smoke:java:ctr") >= 1, "INCR");
            need(r.expire("smoke:java", 100), "EXPIRE");
            long ttl = r.ttl("smoke:java");
            need(ttl > 0 && ttl <= 100, "TTL " + ttl);

            String info = r.info();
            for (String field : new String[] {"redis_version", "connected_clients", "used_memory"}) {
                need(info.contains(field), "INFO missing " + field);
            }

            for (int i = 0; i < 50; i++) r.set("smoke:java:scan:" + i, "x");
            Set<String> seen = new HashSet<>();
            ScanCursor cursor = ScanCursor.INITIAL;
            do {
                KeyScanCursor<String> page =
                        r.scan(cursor, ScanArgs.Builder.matches("smoke:java:scan:*").limit(13));
                seen.addAll(page.getKeys());
                cursor = page;
            } while (!cursor.isFinished());
            need(seen.size() == 50, "SCAN saw " + seen.size() + "/50");

            CountDownLatch got = new CountDownLatch(1);
            StringBuilder payload = new StringBuilder();
            StatefulRedisPubSubConnection<String, String> sub = client.connectPubSub();
            sub.addListener(new RedisPubSubAdapter<String, String>() {
                @Override
                public void message(String channel, String message) {
                    payload.append(message);
                    got.countDown();
                }
            });
            sub.sync().subscribe("smoke:java:chan");
            need(r.publish("smoke:java:chan", "hello") == 1, "PUBLISH receivers");
            need(got.await(5, TimeUnit.SECONDS), "pub/sub delivery timed out");
            need("hello".contentEquals(payload), "pub/sub payload");
            sub.close();
        } finally {
            client.shutdown();
        }
        System.out.println("lettuce smoke: OK");
    }
}
