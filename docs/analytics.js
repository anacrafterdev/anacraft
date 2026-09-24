/* Google Analytics 4 for anacraft.dev, configured to store nothing.
 *
 * client_storage:'none' stops GA4 writing the _ga cookie or touching
 * localStorage. With nothing kept on the visitor's device there is nothing to
 * ask consent for, which is why this site has no cookie banner. The cost is
 * continuity: every visit arrives as a new user, so returning-visitor and
 * multi-day attribution numbers are meaningless here. Page views, countries,
 * referrers, events and Realtime are unaffected.
 *
 * Deliberately no Consent Mode block. Setting analytics_storage:'denied'
 * downgrades every hit to a cookieless consent ping, which GA4 surfaces only
 * through modelling that needs volume thresholds this site will not reach —
 * so it would report less than storing nothing and sending ordinary hits.
 *
 * This covers the website. The anacraft binary has no telemetry of any kind
 * and never phones home; do not let this file blur that line.
 */
(function () {
  var ID = 'G-DYPCVZVMSE';

  // `make serve` previews this site on localhost; that is not a visitor.
  var host = location.hostname;
  if (host !== 'anacraft.dev' && !/\.anacraft\.dev$/.test(host)) return;

  window.dataLayer = window.dataLayer || [];
  function gtag() { dataLayer.push(arguments); }
  window.gtag = gtag;

  gtag('js', new Date());
  gtag('config', ID, { client_storage: 'none' });

  var tag = document.createElement('script');
  tag.async = true;
  tag.src = 'https://www.googletagmanager.com/gtag/js?id=' + ID;
  document.head.appendChild(tag);
})();
