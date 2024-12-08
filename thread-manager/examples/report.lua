done = function(summary, latency, requests)
   x = string.format("%d %d\n", latency.mean, summary.requests)
   io.write(x)
end
