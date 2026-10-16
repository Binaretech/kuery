#!/bin/sh
# Example data for Kuery's Redis driver: every key type, keys that expire,
# enough keys to page through with SCAN, and a second database. REDIS_PORT
# picks the server's port (6379 by default).
set -e

plan() {
  case $(($1 % 3)) in
    0) echo free ;;
    1) echo pro ;;
    *) echo team ;;
  esac
}

{
  echo 'SET app:name "Kuery demo"'
  echo 'SET app:version 1.4.2'
  echo 'SET app:config "{\"theme\":\"dark\",\"features\":{\"profile\":true,\"redis\":true},\"limits\":[10,100,1000]}"'
  echo 'SET counters:visits 48213'
  echo 'SET maintenance:banner "Scheduled maintenance on Sunday at 02:00 UTC" EX 86400'

  i=1
  while [ $i -le 200 ]; do
    echo "HSET user:$i name \"User $i\" email user$i@example.com plan $(plan $i) visits $((i * 7 % 97))"
    echo "ZADD leaderboard $((i * 37 % 1000)) user:$i"
    i=$((i + 1))
  done

  i=1
  while [ $i -le 1500 ]; do
    echo "HSET product:$i name \"Product $i\" price $((i * 37 % 500)).$((i % 100)) category cat-$((i % 12)) stock $((i * 13 % 250))"
    i=$((i + 1))
  done

  i=1
  while [ $i -le 60 ]; do
    echo "SET session:$(printf '%08x' $((i * 2654435761 % 4294967296))) user:$((i % 200 + 1)) EX $((i * 120))"
    echo "SADD online:users user:$((i * 3 % 200 + 1))"
    i=$((i + 1))
  done

  i=1
  while [ $i -le 300 ]; do
    echo "RPUSH queue:emails \"{\\\"to\\\":\\\"user$((i % 200 + 1))@example.com\\\",\\\"template\\\":\\\"welcome\\\",\\\"attempt\\\":$((i % 3))}\""
    i=$((i + 1))
  done

  for tag in rust redis postgres sql gpui desktop analytics charts security audit; do
    echo "SADD tags:all $tag"
  done

  i=1
  while [ $i -le 500 ]; do
    echo "XADD events:orders * order_id $i user user:$((i % 200 + 1)) amount $((i * 17 % 900)).$((i % 100)) status $(plan $i)"
    i=$((i + 1))
  done

  echo 'SELECT 1'
  i=1
  while [ $i -le 40 ]; do
    echo "SET cache:page:/products/$i \"<html>Product $i</html>\" EX $((i * 300))"
    i=$((i + 1))
  done
  echo 'HSET cache:stats hits 9812 misses 412'
} | redis-cli -p "${REDIS_PORT:-6379}" > /dev/null

echo "kuery: Redis seeded"
