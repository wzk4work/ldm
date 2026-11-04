import { test } from "node:test";
import assert from "node:assert/strict";
import { describeReason, groupedOrder, menuEntries, qualityAlternates, scoreAll, type PageSignals, type RankInput } from "../src/shared/adscore";
import { MediaRegistry } from "../src/shared/media";

const DIR = "https://media.site.example/video/tos/";
const HI = `${DIR}hiHIGH/?a=1&br=1041`;
const LO = `${DIR}loLOW/?a=1&br=948`;
const TEASER = "https://static.site.example/webapp/playback1.mp4";

const mseItems = (): RankInput[] => [
  { url: TEASER, kind: "progressive", size: 200_000, firstSeen: 900 },
  { url: LO, kind: "progressive", size: 2_900_000, firstSeen: 1000 },
  { url: HI, kind: "progressive", size: 3_200_000, firstSeen: 1500 },
];
const msePage = (latest: string[]): PageSignals => ({
  players: [
    {
      src: "blob:https://www.site.example/1",
      el: "v1",
      area: 440 * 780,
      played: 20,
      playing: true,
      duration: 25,
      mse: { segments: [HI.split("?")[0], LO.split("?")[0]], latest, codecs: ["avc1.64001f", "mp4a.40.2"] },
    },
  ],
});

test("one MSE player fed from two bitrates: the last-appended file is the main entry, the other its alternate", () => {
  for (const [latest, main, alt] of [
    [[HI.split("?")[0], LO.split("?")[0]], 2, 1],
    [[LO.split("?")[0], HI.split("?")[0]], 1, 2],
  ] as [string[], number, number][]) {
    const items = mseItems();
    const page = msePage(latest);
    const v = scoreAll(items, { page });
    assert.equal(v[1].score, v[2].score, "both are tied by the ad score");
    const a = qualityAlternates(items, v, page);
    assert.equal(a[main], null);
    assert.equal(a[alt], main);
    assert.equal(a[0], null, "the teaser is not grouped");
    assert.deepEqual(groupedOrder([1, 2, 0], a).slice(0, 2), [main, alt]);
  }
});

test("without playback evidence: higher resolution, then bitrate, then size", () => {
  const page: PageSignals = { players: [{ src: "blob:https://www.site.example/1", el: "v1", area: 1, played: 5, duration: 25, mse: { segments: [HI.split("?")[0], LO.split("?")[0]] } }] };
  const items = mseItems();
  const v = scoreAll(items, { page });
  assert.equal(qualityAlternates(items, v, page)[1], 2, "bigger file wins with equal everything else");
  items[1].height = 1080;
  items[2].height = 720;
  assert.equal(qualityAlternates(items, v, page)[2], 1, "resolution beats size");
  items[1].height = items[2].height = 720;
  items[1].duration = 10;
  items[2].duration = 25;
  assert.deepEqual(qualityAlternates(items, v, page), [null, null, null], "different lengths: different videos");
});

test("direct src swap on one element: the src played last is main", () => {
  const A = "https://cdn.example/v/clip_480.mp4";
  const B = "https://cdn.example/v/clip_1080.mp4";
  const items: RankInput[] = [
    { url: A, kind: "progressive", size: 1_000_000, firstSeen: 1 },
    { url: B, kind: "progressive", size: 4_000_000, firstSeen: 2 },
  ];
  const page: PageSignals = {
    players: [
      { src: B, el: "v1", area: 640 * 360, played: 3, firstPlayAt: 10, lastPlayAt: 20 },
      { src: A, el: "v1", area: 640 * 360, played: 3, firstPlayAt: 1, lastPlayAt: 5 },
    ],
  };
  const v = scoreAll(items, { page });
  assert.deepEqual(qualityAlternates(items, v, page), [1, null]);
  // two different elements are two videos
  page.players![1].el = "v2";
  assert.deepEqual(qualityAlternates(items, v, page), [null, null]);
});

test("ads are never grouped; audio-only and different tracks are not alternates", () => {
  const items = mseItems();
  const page = msePage([HI.split("?")[0]]);
  const v = scoreAll(items, { page });
  v[1] = { ...v[1], ad: true };
  assert.deepEqual(qualityAlternates(items, v, page), [null, null, null]);
  const v2 = scoreAll(items, { page });
  items[1].track = "audio";
  items[2].track = "video";
  assert.deepEqual(qualityAlternates(items, v2, page), [null, null, null]);
});

test("hook off: a blob player and files whose MP4 header length matches it", () => {
  const items = mseItems();
  items[1].duration = 25.1;
  items[2].duration = 25.2;
  items[0].duration = 2;
  const page: PageSignals = { players: [{ src: "blob:https://www.site.example/1", el: "v1", area: 1, played: 9, playing: true, duration: 25.17 }] };
  const v = scoreAll(items, { page });
  const a = qualityAlternates(items, v, page);
  assert.equal(a[1], 2);
  assert.equal(a[0], null);
});

test("registry + menu: the alternate sits right under its main entry, tagged, not greyed, reason kept", () => {
  const r = new MediaRegistry();
  for (const it of mseItems()) r.add(7, { url: it.url, kind: "progressive", size: it.size } as never, it.firstSeen);
  const page = msePage([HI.split("?")[0]]);
  r.setSignals(7, page);
  const list = r.list(7, 2000);
  const hi = list.findIndex((i) => i.url === HI);
  const lo = list.findIndex((i) => i.url === LO);
  assert.equal(lo, hi + 1, list.map((i) => i.url).join());
  assert.equal(list[lo].altOf, HI);
  assert.ok(list[lo].adReasons!.includes(`alt-of:${HI}`));
  assert.equal(list[lo].ad, false);
  assert.equal(list[lo].adScore, list[hi].adScore, "the score is unchanged (ad greying untouched)");
  const rows = menuEntries(list, "en");
  const row = rows.find((x) => x.item.url === LO)!;
  assert.ok(row.sub && !row.greyed);
  assert.equal(row.tag, "Alt. quality");
  assert.ok(!rows.find((x) => x.item.url === HI)!.sub);
  assert.equal(describeReason(`alt-of:${HI}`, "en"), "another bitrate of the same video (hiHIGH)");
  assert.ok(r.setMp4Info(7, LO, { duration: 25.2, height: 720 }));
  assert.ok(!r.setMp4Info(7, LO, { duration: 25.2, height: 720 }), "no change the second time");
});
