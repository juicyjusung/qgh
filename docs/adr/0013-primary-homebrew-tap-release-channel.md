# Primary Homebrew Tap Release Channel

qgh will reuse the existing public `juicyjusung/homebrew-tap` repository as the required day-one Homebrew install channel, exposed as `brew install juicyjusung/tap/qgh`, with GitHub Releases as the release artifact origin. This keeps the required Brew path under project control while avoiding a `homebrew/core` review dependency on the first shippable release; `homebrew/core` can remain a later distribution milestone.
