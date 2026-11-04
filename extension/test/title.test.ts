import { test } from "node:test";
import assert from "node:assert/strict";
import { cleanTitle, hostBrand, pickTitle } from "../src/shared/title";

test("page titles lose site names and counters", () => {
  assert.equal(cleanTitle("Some Clip Name - Pornhub.com", "https://www.pornhub.com/view_video.php?viewkey=x"), "Some Clip Name");
  assert.equal(cleanTitle("(3) Never Gonna Fix It - YouTube", "https://www.youtube.com/watch?v=x"), "Never Gonna Fix It");
  assert.equal(cleanTitle("A talk | Vimeo", "https://vimeo.com/1"), "A talk");
  assert.equal(cleanTitle("Report – BBC", "https://www.bbc.co.uk/news/x"), "Report");
  assert.equal(cleanTitle("News report – BBC News", "https://www.bbc.co.uk/news/x"), "News report – BBC News");
  assert.equal(cleanTitle("我的视频标题_哔哩哔哩_bilibili", "https://www.bilibili.com/video/BV1"), "我的视频标题");
  assert.equal(cleanTitle("Example | My Movie", "https://example.org/v/1"), "My Movie");
  assert.equal(cleanTitle("Cats - Dogs - example.com", "https://www.example.com/a"), "Cats - Dogs");
  assert.equal(cleanTitle("Rock - Live 1999", "https://music.example.net/"), "Rock - Live 1999");
  assert.equal(cleanTitle("[12] Inbox clip - Twitch", "https://www.twitch.tv/v"), "Inbox clip");
  assert.equal(cleanTitle("YouTube", "https://www.youtube.com/"), "");
  // real-world: Dailymotion's tab title suffix; generic words alone are kept
  assert.equal(cleanTitle("Passing By | movie | 2026 | Official Trailer - video Dailymotion", "https://www.dailymotion.com/video/x"), "Passing By | movie | 2026 | Official Trailer");
  assert.equal(cleanTitle("Video games - a history", "https://example.com/v"), "Video games - a history");
  // real-world: TikTok's tab title while its sign-up dialog is open, and its og:title
  assert.equal(cleanTitle("Sign up | TikTok", "https://www.tiktok.com/@nasa/video/1"), "");
  assert.equal(cleanTitle("NASA on TikTok", "https://www.tiktok.com/@nasa/video/1"), "NASA");
  assert.equal(cleanTitle("Rock on Broadway", "https://www.tiktok.com/@x/video/1"), "Rock on Broadway");
  assert.equal(cleanTitle("  Plain   title ", null), "Plain title");
  assert.equal(cleanTitle(undefined, "bad url"), "");
  assert.equal(hostBrand("news.bbc.co.uk"), "bbc");
});

test("site-name-only tab titles fall back to og:title, then h1", () => {
  const u = "https://www.example.com/watch?v=1";
  assert.equal(pickTitle("Real Title - Example", u, { og: "Other", h1: "H" }), "Real Title");
  assert.equal(pickTitle("Example", u, { og: "Og Title | Example", h1: "H" }), "Og Title");
  assert.equal(pickTitle("Example", u, { og: null, h1: "  Visible   heading " }), "Visible heading");
  assert.equal(pickTitle("Example.com", u, {}), "");
  assert.equal(pickTitle(null, u, null), "");
});
