# Proxy server

A simple proxy server written in rust. Start it up by running `cargo run`. The proxy server runs, by default, on localhost (127.0.0.1) and on port 3000, and supports basic auth. The username and password are, by default.... username and password. Example usage:

```
curl -x http://localhost:3000 --proxy-user username:password -L https://google.com
```

The proxy supports both http and https, and keeps track of some metrics, including sites visited and how much data's been transfered through the proxy. You can see those metrics by hitting the proxy's `GET /metrics` endpoint, ie:

```
curl localhost:3000/metrics
```

The server also reports these metrics right before it shuts down.
