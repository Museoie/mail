import { Link } from "react-router-dom";
import "../SignIn/index.scss";

/** Any path the app has no page for. */
const NotFound = () => (
  <div className="container_login">
    <div className="login_card not_found">
      <h2 className="greeting">Page not found</h2>
      <p className="info_message">
        There&apos;s nothing at this address.
      </p>
      <Link className="not_found_home" to="/">
        Go to your mail
      </Link>
    </div>
  </div>
);

export default NotFound;
