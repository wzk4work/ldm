/* Sample load-balancing PAC (test fixture): hash the host onto one of three proxies. */
var proxies = ["PROXY p1.example:8080", "PROXY p2.example:8080", "PROXY p3.example:8080"];

function hash(s) {
    var h = 0;
    for (var i = 0; i < s.length; i++) {
        h = (h * 31 + s.charCodeAt(i)) % 65536;
    }
    return h;
}

function FindProxyForURL(url, host) {
    var first = hash(host) % proxies.length;
    var list = [];
    for (var k = 0; k < proxies.length; k++) {
        list.push(proxies[(first + k) % proxies.length]);
    }
    return list.join("; ") + "; DIRECT";
}
