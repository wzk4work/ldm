// Sample corporate PAC file (test fixture).
var LOCAL_NETS = [
    ["10.0.0.0", "255.0.0.0"],
    ["172.16.0.0", "255.240.0.0"],
    ["192.168.0.0", "255.255.0.0"]
];

function isLocal(host) {
    for (var i = 0; i < LOCAL_NETS.length; i++) {
        if (isInNet(host, LOCAL_NETS[i][0], LOCAL_NETS[i][1])) return true;
    }
    return false;
}

function FindProxyForURL(url, host) {
    host = host.toLowerCase();
    if (isPlainHostName(host) || dnsDomainIs(host, ".corp.example") || localHostOrDomainIs(host, "intranet.corp.example"))
        return "DIRECT";
    if (shExpMatch(host, "*.media.example") && url.substring(0, 6) == "https:")
        return "SOCKS5 socks.corp.example:1080; DIRECT";
    if (!isResolvable(host))
        return "DIRECT";
    if (isLocal(host))
        return "DIRECT";
    if (weekdayRange("SAT", "SUN") || timeRange(8, 20))
        return "PROXY proxy1.corp.example:3128; PROXY proxy2.corp.example:3128; DIRECT";
    return "PROXY proxy2.corp.example:3128; DIRECT";
}
