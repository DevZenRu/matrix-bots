# devzen-matrix-topics-bot

## How to get Trello keys

#### TRELLO_APP_KEY 
Get Trello API Key from https://trello.com/app-key

#### TRELLO_READ_TOKEN
Get from: https://trello.com/1/authorize?expiration=never&name=DevZenShownotesGen&key=TRELLO_APP_KEY&scope=read&response_type=token

## How to register a webhook in Trello

```bash
curl -X POST -H "Content-Type: application/json" \
https://api.trello.com/1/tokens/{TRELLO_READ_TOKEN}/webhooks/ \
-d '{
  "key": "{TRELLO_APP_KEY}",
  "callbackURL": "http://{your_domain_and_port}/trellohook",
  "idModel":"{TRELLO_IN_DISCUSSION_LIST_ID}",
  "description": "DevZen_Matrix_Webhook"
}'
```

Read more: https://developer.atlassian.com/cloud/trello/guides/rest-api/webhooks/

