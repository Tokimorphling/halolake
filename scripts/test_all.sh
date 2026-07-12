BASE=http://127.0.0.1:9090
GROUP=temp
MODEL=grok-4.5
AUTH=(-H "Authorization: Bearer $ACCESS_TOKEN")

page=1
ids=()
while :; do
  resp=$(curl -sS "${AUTH[@]}" "$BASE/api/channel?group=$(printf %s "$GROUP"|jq -sRr @uri)&page=$page&page_size=100")
  mapfile -t batch < <(echo "$resp" | jq -r '.data.items[]?.id // empty')
  [[ ${#batch[@]} -eq 0 ]] && break
  ids+=("${batch[@]}")
  total=$(echo "$resp" | jq -r '.data.total // 0')
  [[ ${#ids[@]} -ge $total ]] && break
  page=$((page+1))
done

for id in "${ids[@]}"; do
  r=$(curl -sS "${AUTH[@]}" \
    "$BASE/api/channel/test/${id}?model=$(printf %s "$MODEL"|jq -sRr @uri)&stream=false")
  ok=$(echo "$r" | jq -r '.success')
  t=$(echo "$r" | jq -r '.time // .data.time // 0')
  msg=$(echo "$r" | jq -r '.message // empty')
  echo "id=$id success=$ok time=$t $msg"
done