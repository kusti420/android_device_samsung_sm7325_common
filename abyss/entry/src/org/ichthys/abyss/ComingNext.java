/*
 * Copyright (C) 2026 IchthysOS
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.ichthys.abyss;

import android.content.Intent;
import android.net.Uri;
import android.widget.LinearLayout;
import android.widget.TextView;

/** Advanced › Coming next: Scripts (v1.1) and Apps &amp; containers (v1.2), text only. */
final class ComingNext {
    private ComingNext() {
    }

    /**
     * Title, "coming next · release" tag, body and a "Read more" line. The line is a link only
     * when docs_base_url is set (strings.xml); until the docs are published it is plain dim
     * text, never a dead link.
     */
    static void add(Page p, Page.Group g, int title, String release, int body, int docs,
            int path) {
        LinearLayout col = new LinearLayout(p.a);
        col.setOrientation(LinearLayout.VERTICAL);
        col.addView(p.title(p.a.getString(title)));
        TextView tag = p.summary(p.a.getString(R.string.coming_next, release));
        tag.setTextColor(p.a.getColor(R.color.abyss_warn));
        col.addView(tag, p.lp(2));
        col.addView(p.summary(p.a.getString(body)), p.lp(6));
        TextView more = p.summary(p.a.getString(docs));
        col.addView(more, p.lp(8));
        g.view(col);
        String base = p.a.getString(R.string.docs_base_url).trim();
        if (base.isEmpty()) {
            return;
        }
        Uri uri = Uri.parse(base.replaceAll("/+$", "") + "/" + p.a.getString(path));
        more.setTextColor(p.a.getColor(R.color.abyss_accent));
        more.setClickable(true);
        more.setOnClickListener(v -> p.start(new Intent(Intent.ACTION_VIEW, uri)));
    }
}
