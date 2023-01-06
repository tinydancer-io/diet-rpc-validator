curl http://localhost:8899 -X POST -H "Content-Type: application/json" -d '
  {"jsonrpc": "2.0","id":1,"method":"getBlockHeaders","params":[4, {"encoding": "jsonParsed","maxSupportedTransactionVersion":0,"transactionDetails":"full","rewards":false}]}
'