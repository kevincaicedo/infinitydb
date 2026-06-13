// go-redis smoke vs infinityd (M1-S03/S14): RESP3 negotiation, strings,
// expiry, INFO, CLIENT, SCAN, pub/sub round trip.
//
// Usage: go run . [host:port]
package main

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/redis/go-redis/v9"
)

func need(cond bool, what string) {
	if !cond {
		fmt.Fprintln(os.Stderr, "client-smoke FAILED:", what)
		os.Exit(1)
	}
}

func main() {
	addr := "127.0.0.1:6379"
	if len(os.Args) > 1 {
		addr = os.Args[1]
	}
	ctx := context.Background()
	r := redis.NewClient(&redis.Options{Addr: addr})

	need(r.Ping(ctx).Val() == "PONG", "PING")
	need(r.Set(ctx, "smoke:go", "v", 0).Val() == "OK", "SET")
	need(r.Get(ctx, "smoke:go").Val() == "v", "GET")
	need(r.Incr(ctx, "smoke:go:ctr").Val() >= 1, "INCR")
	need(r.Expire(ctx, "smoke:go", 100*time.Second).Val(), "EXPIRE")
	ttl := r.TTL(ctx, "smoke:go").Val()
	need(ttl > 0 && ttl <= 100*time.Second, "TTL")

	info := r.Info(ctx).Val()
	for _, field := range []string{"redis_version", "connected_clients", "used_memory"} {
		need(strings.Contains(info, field), "INFO missing "+field)
	}

	need(r.Do(ctx, "CLIENT", "SETNAME", "smoke-go").Val() == "OK", "CLIENT SETNAME")
	need(r.Do(ctx, "CLIENT", "GETNAME").Val() == "smoke-go", "CLIENT GETNAME")

	for i := 0; i < 50; i++ {
		r.Set(ctx, fmt.Sprintf("smoke:go:scan:%d", i), "x", 0)
	}
	seen := map[string]bool{}
	iter := r.Scan(ctx, 0, "smoke:go:scan:*", 13).Iterator()
	for iter.Next(ctx) {
		seen[iter.Val()] = true
	}
	need(iter.Err() == nil, "SCAN iteration")
	need(len(seen) == 50, fmt.Sprintf("SCAN saw %d/50", len(seen)))

	sub := r.Subscribe(ctx, "smoke:go:chan")
	_, err := sub.Receive(ctx) // subscribe confirmation
	need(err == nil, "SUBSCRIBE confirmation")
	need(r.Publish(ctx, "smoke:go:chan", "hello").Val() == 1, "PUBLISH receivers")
	msgCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	msg, err := sub.ReceiveMessage(msgCtx)
	need(err == nil && msg.Payload == "hello", "pub/sub delivery")
	_ = sub.Close()

	fmt.Println("go-redis smoke: OK")
}
