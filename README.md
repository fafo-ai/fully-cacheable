
<h1>
  <img width="55px" src="https://github.com/user-attachments/assets/c689c8d7-5c05-4c05-8247-49f976a1036d" /> 
  <span>fully-cacheable - OpenAI Proxy</span>

  <a href="https://github.com/fafo-ai/fully-cacheable/actions/workflows/rust.yml">
    <img src="https://github.com/fafo-ai/fully-cacheable/actions/workflows/rust.yml/badge.svg?branch=main" />
  </a>
</h1>

Call this instead of OpenAI's API - and have your embeddings automatically cached.

Written in Rust.

# Getting started
To use `fully-cacheable`, simply substitute OpenAI's `base_url` by a fully-cacheable server. For example in Python:

```
openai_client = openai.Client(base_url="https://fully-cacheable.fly.dev/v1")

openai_client = openai.Client(base_url="http://localhost:4567/v1")
```

the top URL is hosted by us - but feel free to use it. The bottom one assumes you're running a `fully-cacheable` server locally.


Per [the code](https://github.com/fafo-ai/fully-cacheable/blob/main/src/main.rs#L25), `fully-cacheable` doesn't store any of the data that you send to it. Only hashes.

# Building and running fully-cacheable locally
```cargo run --package fully-cacheable --bin fully-cacheable```




<img style="margin-top: 200px" width="250px" src="https://github.com/user-attachments/assets/f1602dba-55f8-42ba-85ed-ce52439e2c14" />

