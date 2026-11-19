(() => {
  'use strict';
  globalThis.VDSiteMedia = {
    inspect(video) {
      let record = null;
      const receive = (event) => {
        if (typeof event.detail !== 'string' || event.detail.length > 1024 * 1024) return;
        try {
          const value = JSON.parse(event.detail);
          if (Array.isArray(value.formats)) record = value;
        } catch { /* Ignore unrelated page events. */ }
      };
      video.addEventListener('vd-player-formats-v1', receive);
      try { video.dispatchEvent(new Event('vd-inspect-player-v1', {bubbles: true})); }
      finally { video.removeEventListener('vd-player-formats-v1', receive); }
      return record;
    }
  };
})();
