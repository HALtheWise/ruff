from django.urls import path, re_path

from . import views

urlpatterns = [
    path("orders/", views.orders),
    path("orders/<int:id>/", views.order),
    path("customers/", views.customers),
    path("orders/", views.legacy_orders),
    re_path(r"^customers/$", views.customers),
    path(route="customers/", view=views.customers),
]
