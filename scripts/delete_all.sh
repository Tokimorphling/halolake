BASE=http://127.0.0.1:9090
GROUP=temp
TOKEN=$ACCESS_TOKEN

# 1) 收集该 group 全部 id
ids=[]
page=1
while :; do
  resp=$(curl -sS -H "Authorization: Bearer $TOKEN" \
    "$BASE/api/channel?group=$(printf %s "$GROUP"|jq -sRr @uri)&page=$page&page_size=200")
  batch=$(echo "$resp" | jq -c '[.data.items[]?.id]')
  n=$(echo "$batch" | jq 'length')
  [[ "$n" -eq 0 ]] && break
  ids=$(jq -nc --argjson a "$ids" --argjson b "$batch" '$a+$b')
  total=$(echo "$resp" | jq -r '.data.total // 0')
  got=$(echo "$ids" | jq 'length')
  echo "page $page: +$n ($got/$total)"
  [[ "$got" -ge "$total" ]] && break
  page=$((page+1))
done
echo "will delete $(echo "$ids" | jq 'length') channels: $ids"

# 2) 确认后删除（先 dry-run 看上面列表，再执行下面）
curl -sS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -X POST "$BASE/api/channel/batch" \
  -d "$(jq -nc --argjson ids "$ids" '{ids:$ids}')"